//! The For You feed: a browser opens `tiktok.com/foryou` for one batch, and
//! tuitok keeps the videos the page asks TikTok for (the answers to
//! `/api/recommend/item_list/` and `/api/preload/item_list/`, read with
//! `Network.getResponseBody`), and the guest `tt_chain_token` cookie their
//! addresses need. Pictures, fonts, video and sound are refused and `play()`
//! does nothing: TikTok streams through MediaSource with fetch, so refusing
//! "Media" alone stops nothing (the report's "What it costs").
//!
//! A batch hands on each answer's videos as they come (`Tell`), once the
//! cookie is in: the page's first answer comes about 1.5 s after it opens.
//! Batches grow as `SIZES` says, and each carries the guest's cookies to the
//! next (`Ask::cookies`): a new guest gets the same popular videos again
//! (four batches of 13 from new guests had 28 different videos of 52).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::browser::{self, Browser, Engine};
use crate::cdp::Cdp;
use crate::text;
use crate::tools::Tools;
use crate::video::{Video, VideoId};

pub const URL: &str = "https://www.tiktok.com/foryou";

/// How many new videos each batch of a run asks for, one after another: one
/// first, so it plays soonest (Lightpanda: 1.6 s, 0.7 s of CPU), then more,
/// then `BATCH` for every batch after (10 s, 2.4 s of CPU).
pub const SIZES: [usize; 6] = [1, 2, 3, 5, 8, 13];
pub const BATCH: usize = SIZES[SIZES.len() - 1];

/// The size of batch `n` of a run, from 0.
pub fn batch_size(n: usize) -> usize {
    SIZES[n.min(SIZES.len() - 1)]
}

/// The most cookies carried to the next batch, and the longest value.
const MAX_COOKIES: usize = 64;
const MAX_COOKIE: usize = 4096;

/// The longest a batch looks: then it ends with what it has.
const LIMIT: Duration = Duration::from_secs(40);

/// Chrome loads more only when scrolled: ArrowDown after this long without
/// a new answer. Lightpanda loads more by itself.
const PRESS_AFTER: Duration = Duration::from_secs(2);

/// A batch with at least `ENOUGH` videos ends once the page has asked for
/// no more for `QUIET` (Lightpanda's answers came at most 4.5 s apart, then
/// stopped): some items aren't videos, so `want` may never be reached.
const ENOUGH: usize = 10;
const QUIET: Duration = Duration::from_secs(6);

/// Makes `play()` do nothing, before the page's own scripts run.
const NO_PLAY: &str =
    "HTMLMediaElement.prototype.play = function () { return Promise.resolve(); };";

/// Where a batch hands on its videos as the page gets them, each with the
/// cookie.
pub type Tell<'a> = &'a (dyn Fn(Vec<Video>) + Send + Sync);

/// What a batch asks for.
#[derive(Clone, Default)]
pub struct Ask {
    /// New videos: once it has these, the batch ends.
    pub want: usize,
    /// The videos the run has: they aren't counted or handed on again.
    pub seen: HashSet<VideoId>,
    /// The guest's cookies from the batch before (`Batch::cookies`): with
    /// them TikTok goes on with that guest's feed.
    pub cookies: Vec<Value>,
}

/// One batch of the feed.
pub struct Batch {
    /// All of them, up to `want`, handed on before through `Tell`.
    pub videos: Vec<Video>,
    /// Videos past `want` in the page's last answer, each with the cookie:
    /// the next batch's first.
    pub spare: Vec<Video>,
    /// The guest's cookies, for the next batch (`Ask::cookies`): only in
    /// memory, only TikTok's.
    pub cookies: Vec<Value>,
    /// The guest cookie the videos' addresses need, if the browser got one.
    pub token: Option<Arc<str>>,
    pub engine: Engine,
    pub took: Duration,
    /// How long until the first videos were handed on.
    pub first: Option<Duration>,
    /// Why an engine before it failed, if one did.
    pub note: Option<String>,
}

/// The browsers to try, in order: Lightpanda, then Chrome; only `only` if
/// it's given. Each with its program.
pub fn engines(tools: &Tools, only: Option<Engine>) -> Vec<(Engine, PathBuf)> {
    [Engine::Lightpanda, Engine::Chrome]
        .into_iter()
        .filter(|engine| only.is_none_or(|only| only == *engine))
        .filter_map(|engine| Some((engine, browser::program(tools, engine)?.to_path_buf())))
        .collect()
}

/// A batch from the first of `engines` that gets one, saying in its `note`
/// why any before it didn't.
pub async fn fetch_any(
    engines: Vec<(Engine, PathBuf)>,
    profile: PathBuf,
    ask: Ask,
    tell: Tell<'_>,
) -> Result<Batch> {
    let mut failed = Vec::new();
    for (engine, program) in engines {
        match fetch(engine, &program, &profile, &ask, tell).await {
            Ok(mut batch) if !batch.videos.is_empty() => {
                batch.note = (!failed.is_empty()).then(|| failed.join("; "));
                return Ok(batch);
            }
            Ok(_) => failed.push(format!("{} got no videos", engine.name())),
            Err(e) => failed.push(format!(
                "{}: {}",
                engine.name(),
                text::one_line(&format!("{e:#}"), 200)
            )),
        }
    }
    if failed.is_empty() {
        bail!(
            "no browser for the feed: brew install lightpanda-io/browser/lightpanda, or install Chrome"
        );
    }
    bail!("{}", failed.join("; "))
}

/// One batch with `engine`: the browser is started, read and closed. If it
/// fails after some videos came, the batch is those.
pub async fn fetch(
    engine: Engine,
    program: &Path,
    profile: &Path,
    ask: &Ask,
    tell: Tell<'_>,
) -> Result<Batch> {
    let started = Instant::now();
    let mut browser = Browser::start(engine, program, profile).await?;
    let mut found = Found::new(tell, started, ask);
    let read = tokio::time::timeout(
        LIMIT + Duration::from_secs(15),
        read(&mut browser, ask, &mut found),
    )
    .await;
    browser.close().await;
    // What came without the cookie: the addresses may refuse them.
    found.hand_on();
    let mut videos = found.videos;
    let mut spare = videos.split_off(found.want.min(videos.len()));
    for video in &mut spare {
        video.token = found.token.clone();
    }
    match read.map_err(|_| anyhow!("took too long")).and_then(|r| r) {
        Err(e) if videos.is_empty() => Err(e),
        _ => Ok(Batch {
            videos,
            spare,
            cookies: found.cookies,
            token: found.token,
            engine,
            took: started.elapsed(),
            first: found.first,
            note: None,
        }),
    }
}

/// What a batch found so far, and how much of it was handed on.
struct Found<'a> {
    want: usize,
    videos: Vec<Video>,
    ids: HashSet<VideoId>,
    token: Option<Arc<str>>,
    cookies: Vec<Value>,
    told: usize,
    tell: Tell<'a>,
    started: Instant,
    first: Option<Duration>,
}

impl<'a> Found<'a> {
    fn new(tell: Tell<'a>, started: Instant, ask: &Ask) -> Self {
        Self {
            want: ask.want,
            videos: Vec::new(),
            ids: ask.seen.clone(),
            token: None,
            cookies: Vec::new(),
            told: 0,
            tell,
            started,
            first: None,
        }
    }

    /// Keeps the videos not seen before in this run.
    fn add(&mut self, videos: Vec<Video>) {
        for video in videos {
            if self.ids.insert(video.id.clone()) {
                self.videos.push(video);
            }
        }
    }

    /// Hands on the videos not handed on yet, up to `want`, each with the
    /// cookie.
    fn hand_on(&mut self) {
        let up_to = self.videos.len().min(self.want);
        if self.told >= up_to {
            return;
        }
        let token = &self.token;
        let new: Vec<Video> = self.videos[self.told..up_to]
            .iter_mut()
            .map(|video| {
                video.token = token.clone();
                video.clone()
            })
            .collect();
        self.told = up_to;
        self.first.get_or_insert(self.started.elapsed());
        (self.tell)(new);
    }
}

async fn read(browser: &mut Browser, ask: &Ask, found: &mut Found<'_>) -> Result<()> {
    let want = ask.want;
    let engine = browser.engine;
    let cdp = &browser.cdp;
    let events = &mut browser.events;
    let target = cdp
        .call("Target.createTarget", json!({"url": "about:blank"}), None)
        .await?;
    let target = target["targetId"].as_str().context("no page")?;
    let attached = cdp
        .call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            None,
        )
        .await?;
    let session = attached["sessionId"]
        .as_str()
        .context("no session")?
        .to_owned();
    let s = Some(session.as_str());
    cdp.call("Network.enable", json!({}), s).await?;
    if engine == Engine::Chrome {
        let version = cdp.call("Browser.getVersion", json!({}), None).await?;
        if let Some(agent) = version["userAgent"].as_str() {
            let agent = agent.replace("HeadlessChrome", "Chrome");
            cdp.call(
                "Network.setUserAgentOverride",
                json!({"userAgent": agent}),
                s,
            )
            .await?;
        }
    }
    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": NO_PLAY}),
        s,
    )
    .await?;
    cdp.call("Fetch.enable", json!({"patterns": refused()}), s)
        .await?;
    if !ask.cookies.is_empty() {
        // Refused, it's a new guest: no worse than without.
        let cookies = json!({"cookies": ask.cookies});
        let _ = cdp.call("Network.setCookies", cookies, s).await;
    }
    cdp.call("Page.navigate", json!({"url": URL}), s).await?;

    let mut feed_requests = HashSet::new();
    let deadline = Instant::now() + LIMIT;
    let mut quiet_since = Instant::now();
    let mut last_answer = Instant::now();
    while found.videos.len() < want {
        let now = Instant::now();
        if now >= deadline
            || (found.videos.len() >= ENOUGH.min(want) && now.duration_since(last_answer) >= QUIET)
        {
            break;
        }
        if engine == Engine::Chrome && now.duration_since(quiet_since) >= PRESS_AFTER {
            press_down(cdp, s);
            quiet_since = now;
        }
        let event = match tokio::time::timeout(PRESS_AFTER.min(deadline - now), events.recv()).await
        {
            Ok(Some(event)) => event,
            Ok(None) => bail!("{} closed", engine.name()),
            Err(_) => continue,
        };
        if event.session.as_deref() != Some(session.as_str()) {
            continue;
        }
        let p = &event.params;
        match event.method.as_str() {
            "Fetch.requestPaused" => answer_paused(cdp, p, s),
            "Network.responseReceived" if is_feed(p["response"]["url"].as_str().unwrap_or("")) => {
                if let Some(id) = p["requestId"].as_str() {
                    feed_requests.insert(id.to_owned());
                }
            }
            "Network.loadingFinished" => {
                let Some(id) = p["requestId"].as_str() else {
                    continue;
                };
                if !feed_requests.remove(id) {
                    continue;
                }
                let Ok(body) = cdp
                    .call("Network.getResponseBody", json!({"requestId": id}), s)
                    .await
                else {
                    continue;
                };
                found.add(parse_body(&body));
                if found.token.is_none() {
                    found.token = cookies(cdp, s)
                        .await
                        .and_then(|c| chain_token(&c))
                        .map(Arc::from);
                }
                if found.token.is_some() {
                    found.hand_on();
                }
                quiet_since = Instant::now();
                last_answer = quiet_since;
            }
            _ => {}
        }
    }
    if let Some(cookies) = cookies(cdp, s).await {
        if found.token.is_none() {
            found.token = chain_token(&cookies).map(Arc::from);
        }
        found.cookies = guest_cookies(&cookies);
    }
    Ok(())
}

/// The page's cookies for TikTok (`Network.getCookies`).
async fn cookies(cdp: &Cdp, session: Option<&str>) -> Option<Value> {
    let urls = json!({"urls": ["https://www.tiktok.com"]});
    cdp.call("Network.getCookies", urls, session).await.ok()
}

/// What's refused: pictures, fonts and media before they're asked for, and
/// fetch and XHR answers once their headers are in, if they hold video or
/// sound ([`answer_paused`]).
fn refused() -> Value {
    json!([
        {"urlPattern": "*", "resourceType": "Image"},
        {"urlPattern": "*", "resourceType": "Font"},
        {"urlPattern": "*", "resourceType": "Media"},
        {"urlPattern": "*", "resourceType": "Fetch", "requestStage": "Response"},
        {"urlPattern": "*", "resourceType": "XHR", "requestStage": "Response"},
    ])
}

/// Refuses a paused request that's a picture, font or media, or an answer
/// holding video or sound; lets the rest (the feed's JSON) go on.
fn answer_paused(cdp: &Cdp, params: &Value, session: Option<&str>) {
    let Some(id) = params["requestId"].as_str() else {
        return;
    };
    let kind = params["responseHeaders"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|h| {
            h["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case("content-type"))
        })
        .and_then(|h| h["value"].as_str())
        .unwrap_or("");
    let answered = params.get("responseStatusCode").is_some();
    if !answered || kind.starts_with("video/") || kind.starts_with("audio/") {
        cdp.send(
            "Fetch.failRequest",
            json!({"requestId": id, "errorReason": "BlockedByClient"}),
            session,
        );
    } else {
        cdp.send("Fetch.continueRequest", json!({"requestId": id}), session);
    }
}

/// Presses ArrowDown in the page, as someone scrolling would.
fn press_down(cdp: &Cdp, session: Option<&str>) {
    for kind in ["keyDown", "keyUp"] {
        let key = json!({"type": kind, "key": "ArrowDown", "code": "ArrowDown", "windowsVirtualKeyCode": 40});
        cdp.send("Input.dispatchKeyEvent", key, session);
    }
}

/// Whether `url` is one of the feed's answers.
fn is_feed(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or("");
    path.starts_with("https://www.tiktok.com/")
        && (path.contains("/api/recommend/item_list") || path.contains("/api/preload/item_list"))
}

/// The videos in one of the feed's answers (`Network.getResponseBody`).
fn parse_body(body: &Value) -> Vec<Video> {
    if body["base64Encoded"].as_bool() == Some(true) {
        return Vec::new();
    }
    let Some(Ok(json)) = body["body"].as_str().map(serde_json::from_str::<Value>) else {
        return Vec::new();
    };
    json["itemList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Video::parse)
        .collect()
}

/// The `tt_chain_token` cookie, if it's there and looks like one: what's
/// sent back in a `Cookie` header must not break it.
fn chain_token(cookies: &Value) -> Option<String> {
    let value = cookies["cookies"]
        .as_array()?
        .iter()
        .find(|c| c["name"].as_str() == Some("tt_chain_token"))?["value"]
        .as_str()?;
    fits_header(value, 512).then(|| value.to_owned())
}

/// Whether `s` can go in a `Cookie` header as it is: 1 to `max` bytes, none
/// that would end or break it.
fn fits_header(s: &str, max: usize) -> bool {
    let fits = |b: u8| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'"' | b'\\');
    (1..=max).contains(&s.len()) && s.bytes().all(fits)
}

/// The cookies to carry to the next batch (`Network.getCookies`' answer),
/// as `Network.setCookies` takes them: only TikTok's, names and values fit
/// for a header, at most `MAX_COOKIES`.
pub fn guest_cookies(cookies: &Value) -> Vec<Value> {
    let tiktok = |domain: &str| {
        let host = domain.strip_prefix('.').unwrap_or(domain);
        host == "tiktok.com" || host.ends_with(".tiktok.com")
    };
    cookies["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let (name, value) = (c["name"].as_str()?, c["value"].as_str()?);
            let domain = c["domain"].as_str().filter(|d| tiktok(d))?;
            let name_fits = fits_header(name, 256) && !name.contains('=');
            if !name_fits || !fits_header(value, MAX_COOKIE) {
                return None;
            }
            let path = c["path"].as_str().filter(|p| p.starts_with('/'));
            let mut param = json!({
                "name": name,
                "value": value,
                "domain": domain,
                "path": path.unwrap_or("/"),
                "secure": c["secure"].as_bool().unwrap_or(false),
                "httpOnly": c["httpOnly"].as_bool().unwrap_or(false),
            });
            if let Some(expires) = c["expires"].as_f64().filter(|e| *e > 0.0) {
                param["expires"] = json!(expires);
            }
            if let Some(same) = c["sameSite"]
                .as_str()
                .filter(|s| matches!(*s, "Strict" | "Lax" | "None"))
            {
                param["sameSite"] = json!(same);
            }
            Some(param)
        })
        .take(MAX_COOKIES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_feeds_answers_are_read() {
        assert!(is_feed(
            "https://www.tiktok.com/api/recommend/item_list/?aid=1988&count=8"
        ));
        assert!(is_feed("https://www.tiktok.com/api/preload/item_list/?x=1"));
        assert!(!is_feed(
            "https://www.tiktok.com/api/feedback/v1/newest_reply/"
        ));
        assert!(!is_feed("https://evil.com/api/recommend/item_list/"));
        assert!(!is_feed(
            "https://evil.com/?https://www.tiktok.com/api/recommend/item_list/"
        ));
    }

    #[test]
    fn bodies_and_cookies() {
        let item =
            json!({"id": "1", "video": {"playAddr": "https://v16-webapp-prime.tiktok.com/x"}});
        let body = json!({"body": json!({"itemList": [item, {"id": "x"}]}).to_string(), "base64Encoded": false});
        assert_eq!(parse_body(&body).len(), 1);
        assert!(parse_body(&json!({"body": "e30=", "base64Encoded": true})).is_empty());
        assert!(parse_body(&json!({"body": "not json"})).is_empty());
        let cookies = |value: &str| json!({"cookies": [{"name": "ttwid", "value": "a"}, {"name": "tt_chain_token", "value": value}]});
        assert_eq!(chain_token(&cookies("AbC+/=9")).as_deref(), Some("AbC+/=9"));
        assert_eq!(chain_token(&cookies("a;b")), None);
        assert_eq!(chain_token(&cookies("")), None);
        assert_eq!(chain_token(&json!({"cookies": []})), None);
    }

    #[test]
    fn only_tiktoks_cookies_are_carried() {
        let cookies = json!({"cookies": [
            {"name": "ttwid", "value": "1%7Cab=", "domain": ".tiktok.com", "path": "/", "secure": true, "httpOnly": true, "expires": 1.8e9, "sameSite": "None", "size": 9},
            {"name": "msToken", "value": "x", "domain": "www.tiktok.com", "session": true, "expires": -1},
            {"name": "a", "value": "b", "domain": "evil-tiktok.com"},
            {"name": "a", "value": "b", "domain": ".tiktok.com.evil.com"},
            {"name": "bad", "value": "a;b", "domain": ".tiktok.com"},
            {"name": "a=b", "value": "c", "domain": ".tiktok.com"},
            {"name": "empty", "value": "", "domain": ".tiktok.com"},
        ]});
        let carried = guest_cookies(&cookies);
        assert_eq!(
            carried,
            [
                json!({"name": "ttwid", "value": "1%7Cab=", "domain": ".tiktok.com", "path": "/", "secure": true, "httpOnly": true, "expires": 1.8e9, "sameSite": "None"}),
                json!({"name": "msToken", "value": "x", "domain": "www.tiktok.com", "path": "/", "secure": false, "httpOnly": false}),
            ]
        );
        let many: Vec<Value> = (0..100)
            .map(|n| json!({"name": format!("c{n}"), "value": "v", "domain": ".tiktok.com"}))
            .collect();
        assert_eq!(guest_cookies(&json!({"cookies": many})).len(), MAX_COOKIES);
    }

    #[test]
    fn batches_grow_then_stay() {
        let sizes: Vec<usize> = (0..8).map(batch_size).collect();
        assert_eq!(sizes, [1, 2, 3, 5, 8, 13, 13, 13]);
    }

    /// Against TikTok, with each browser found: `cargo test -- --ignored live`.
    #[tokio::test]
    #[ignore]
    async fn live_feed() {
        let tools = Tools::find();
        let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/live-profile");
        for (engine, program) in engines(&tools, None) {
            let told = std::sync::Mutex::new(0);
            let tell = |videos: Vec<Video>| {
                assert!(videos.iter().all(|v| v.token.is_some()));
                *told.lock().unwrap() += videos.len();
            };
            let ask = Ask {
                want: 10,
                ..Ask::default()
            };
            let batch = fetch(engine, &program, &profile, &ask, &tell)
                .await
                .unwrap();
            assert_eq!(*told.lock().unwrap(), batch.videos.len());
            assert!(batch.videos.len() <= 10);
            assert!(!batch.videos.is_empty(), "{} got no videos", engine.name());
            assert!(
                batch.token.is_some(),
                "{} got no tt_chain_token",
                engine.name()
            );
        }
        let _ = std::fs::remove_dir_all(profile);
    }

    /// How many of a second batch's videos the first had, with and without
    /// the first's cookies: `cargo test -- --ignored live_guest --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_guest_goes_on() {
        let tools = Tools::find();
        let Some((engine, program)) = engines(&tools, Some(Engine::Lightpanda)).pop() else {
            return;
        };
        let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/live-profile");
        let ask = Ask {
            want: BATCH,
            ..Ask::default()
        };
        let ids = |b: &Batch| {
            b.videos
                .iter()
                .map(|v| v.id.clone())
                .collect::<HashSet<_>>()
        };
        for round in 0..2 {
            let first = fetch(engine, &program, &profile, &ask, &|_| {})
                .await
                .unwrap();
            assert!(!first.cookies.is_empty());
            let fresh = fetch(engine, &program, &profile, &ask, &|_| {})
                .await
                .unwrap();
            let carried = Ask {
                cookies: first.cookies.clone(),
                ..ask.clone()
            };
            let on = fetch(engine, &program, &profile, &carried, &|_| {})
                .await
                .unwrap();
            let (a, b, c) = (ids(&first), ids(&fresh), ids(&on));
            eprintln!(
                "round {round}: a new guest repeated {} of {}, the same guest {} of {} ({} cookies)",
                a.intersection(&b).count(),
                b.len(),
                a.intersection(&c).count(),
                c.len(),
                first.cookies.len()
            );
        }
        let _ = std::fs::remove_dir_all(profile);
    }
}
