//! The For You feed: a browser opens `tiktok.com/foryou` for one batch, and
//! tuitok keeps the videos the page asks TikTok for (the answers to
//! `/api/recommend/item_list/` and `/api/preload/item_list/`, read with
//! `Network.getResponseBody`), and the guest `tt_chain_token` cookie their
//! addresses need. Pictures, fonts, video and sound are refused and `play()`
//! does nothing: TikTok streams through MediaSource with fetch, so refusing
//! "Media" alone stops nothing (the report's "What it costs").

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::browser::{self, Browser, Engine};
use crate::cdp::Cdp;
use crate::text;
use crate::tools::Tools;
use crate::video::Video;

pub const URL: &str = "https://www.tiktok.com/foryou";

/// Videos a batch asks for: Lightpanda had about 35 in 17 s.
pub const WANT: usize = 30;

/// The longest a batch looks: then it ends with what it has.
const LIMIT: Duration = Duration::from_secs(40);

/// Chrome loads more only when scrolled: ArrowDown after this long without
/// a new answer. Lightpanda loads more by itself.
const PRESS_AFTER: Duration = Duration::from_secs(2);

/// A batch with at least `ENOUGH` videos ends once the page has asked for
/// no more for `QUIET` (Lightpanda's answers came at most 4.5 s apart, then
/// stopped): some items aren't videos, so `WANT` may never be reached.
const ENOUGH: usize = 10;
const QUIET: Duration = Duration::from_secs(6);

/// Makes `play()` do nothing, before the page's own scripts run.
const NO_PLAY: &str =
    "HTMLMediaElement.prototype.play = function () { return Promise.resolve(); };";

/// One batch of the feed.
pub struct Batch {
    pub videos: Vec<Video>,
    /// The guest cookie the videos' addresses need, if the browser got one.
    pub token: Option<String>,
    pub engine: Engine,
    pub took: Duration,
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
    want: usize,
) -> Result<Batch> {
    let mut failed = Vec::new();
    for (engine, program) in engines {
        match fetch(engine, &program, &profile, want).await {
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

/// One batch with `engine`: the browser is started, read and closed.
pub async fn fetch(engine: Engine, program: &Path, profile: &Path, want: usize) -> Result<Batch> {
    let started = Instant::now();
    let mut browser = Browser::start(engine, program, profile).await?;
    let read =
        tokio::time::timeout(LIMIT + Duration::from_secs(15), read(&mut browser, want)).await;
    browser.close().await;
    let (videos, token) = read.map_err(|_| anyhow!("took too long"))??;
    Ok(Batch {
        videos,
        token,
        engine,
        took: started.elapsed(),
        note: None,
    })
}

async fn read(browser: &mut Browser, want: usize) -> Result<(Vec<Video>, Option<String>)> {
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
    cdp.call("Page.navigate", json!({"url": URL}), s).await?;

    let mut feed_requests = HashSet::new();
    let mut videos: Vec<Video> = Vec::new();
    let mut ids = HashSet::new();
    let deadline = Instant::now() + LIMIT;
    let mut quiet_since = Instant::now();
    let mut last_answer = Instant::now();
    while videos.len() < want {
        let now = Instant::now();
        if now >= deadline
            || (videos.len() >= ENOUGH.min(want) && now.duration_since(last_answer) >= QUIET)
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
                for video in parse_body(&body) {
                    if ids.insert(video.id.clone()) {
                        videos.push(video);
                    }
                }
                quiet_since = Instant::now();
                last_answer = quiet_since;
            }
            _ => {}
        }
    }
    let cookies = cdp
        .call(
            "Network.getCookies",
            json!({"urls": ["https://www.tiktok.com"]}),
            s,
        )
        .await?;
    Ok((videos, chain_token(&cookies)))
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
    let fits = |b: u8| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'"' | b'\\');
    ((1..=512).contains(&value.len()) && value.bytes().all(fits)).then(|| value.to_owned())
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

    /// Against TikTok, with each browser found: `cargo test -- --ignored live`.
    #[tokio::test]
    #[ignore]
    async fn live_feed() {
        let tools = Tools::find();
        let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/live-profile");
        for (engine, program) in engines(&tools, None) {
            let batch = fetch(engine, &program, &profile, 10).await.unwrap();
            assert!(!batch.videos.is_empty(), "{} got no videos", engine.name());
            assert!(
                batch.token.is_some(),
                "{} got no tt_chain_token",
                engine.name()
            );
        }
        let _ = std::fs::remove_dir_all(profile);
    }
}
