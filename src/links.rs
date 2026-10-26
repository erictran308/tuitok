//! A TikTok video's link, typed (`:open`), pasted, or given to `tuitok
//! LINK`: only TikTok's own hosts count, and only the video's id (or a short
//! link's code) is kept. What tuitok then fetches is built from it, never
//! the link itself, so a link can't send anything elsewhere (tuitube's
//! `links.rs`).
//!
//! A video's page answers a plain request (measured 2026-10-17: `reports/Search
//! and links.md`): `GET https://www.tiktok.com/@_/video/ID` holds the video
//! in `__UNIVERSAL_DATA_FOR_REHYDRATION__`, in the feed's shape, and sets
//! `tt_chain_token`, which its addresses need; 0.7 to 0.8 s, about 420 KB.
//! A short link (`vt.tiktok.com/…`) is one redirect to it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{LOCATION, USER_AGENT};
use serde_json::Value;

use crate::cache;
use crate::explore;
use crate::video::{self, Video, VideoId};

/// The longest text read as a link: a shared one with its query is under
/// 300 characters.
pub const MAX_LINK: usize = 2_000;
/// The longest short link code taken (TikTok's are 9).
const MAX_CODE: usize = 32;
/// The fewest digits of a video's id alone: TikTok's ids hold when they
/// were made in their top 32 bits, 19 digits since 2016 (20 from 2043), so
/// a stray number pasted isn't taken for one.
const BARE_ID: usize = 19;
/// The biggest video page read: they were 416 to 432 KB.
const MAX_BYTES: usize = 4 * 1024 * 1024;
/// How long a request may take (one page took 6.7 s).
const WAIT: Duration = Duration::from_secs(15);

/// What a link is to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Video(VideoId),
    /// A short link, rebuilt from its code on its own host: where it leads
    /// is asked when it's opened ([`fetch`]).
    Short(String),
}

/// `text` as a TikTok video's link, if it's one: `tiktok.com/@user/video/ID`
/// on `www.`, `m.` or no host prefix, `m.tiktok.com/v/ID.html`,
/// `/embed/ID`, `/embed/v2/ID`, `/player/v1/ID`; the short
/// `vm.tiktok.com/CODE`, `vt.tiktok.com/CODE` and `tiktok.com/t/CODE`; with
/// or without `https://`; or a video's id alone (at least `BARE_ID`
/// digits). Not photo posts, which have no video.
pub fn parse(text: &str) -> Option<Link> {
    let text = text.trim();
    if text.len() > MAX_LINK || text.chars().any(char::is_whitespace) {
        return None;
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        return VideoId::new(text)
            .filter(|_| text.len() >= BARE_ID)
            .map(Link::Video);
    }
    let with_scheme = if text.contains("://") {
        text.to_owned()
    } else {
        format!("https://{text}")
    };
    let url = reqwest::Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let path: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let video = |id: &str| VideoId::new(id).map(Link::Video);
    let short = |on: &str, code: &str| {
        let fits = (1..=MAX_CODE).contains(&code.len())
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        fits.then(|| Link::Short(format!("https://{on}/{code}/")))
    };
    match (host, path.as_slice()) {
        ("vm.tiktok.com" | "vt.tiktok.com", [code]) => short(host, code),
        ("tiktok.com" | "www.tiktok.com" | "m.tiktok.com", path) => match path {
            ["t", code] => short("www.tiktok.com/t", code),
            [user, "video", id] if user.starts_with('@') => video(id),
            ["v", page] => video(page.strip_suffix(".html").unwrap_or(page)),
            ["embed", id] | ["embed", "v2", id] | ["player", "v1", id] => video(id),
            _ => None,
        },
        _ => None,
    }
}

/// Where video `id`'s page is: `@_` stands for anyone, as in yt-dlp.
pub fn page_url(id: &VideoId) -> String {
    format!("https://www.tiktok.com/@_/video/{}", id.as_str())
}

/// The video `link` is to, read from its page with `client` (TikTok's
/// `www` host only), with the `tt_chain_token` its page set: a new guest
/// each time, sent no cookie. A short link is asked where it leads with
/// `short_client`, which follows no redirect (`cache::client(|_| false)`).
pub async fn fetch(
    client: &reqwest::Client,
    short_client: &reqwest::Client,
    link: Link,
) -> Result<Video> {
    let id = match link {
        Link::Video(id) => id,
        Link::Short(url) => resolve(short_client, &url).await?,
    };
    let response = client
        .get(page_url(&id))
        .header(USER_AGENT, cache::USER_AGENT)
        .timeout(WAIT)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("its page: HTTP {}", response.status().as_u16());
    }
    let token = explore::cookie_set(response.headers(), "tt_chain_token");
    let data = cache::read_capped(response, MAX_BYTES, "its page").await?;
    let mut video = read_page(&String::from_utf8_lossy(&data), &id)?;
    video.token = Some(Arc::from(
        token.context("TikTok made no guest (no tt_chain_token)")?,
    ));
    Ok(video)
}

/// Where short link `url` leads: its redirect (`Location`), not followed
/// (`client` follows none) but read as a link itself, which must be to a
/// video.
async fn resolve(client: &reqwest::Client, url: &str) -> Result<VideoId> {
    let response = client
        .get(url)
        .header(USER_AGENT, cache::USER_AGENT)
        .timeout(WAIT)
        .send()
        .await?;
    if !response.status().is_redirection() {
        bail!("the short link: HTTP {}", response.status().as_u16());
    }
    let to = response
        .headers()
        .get(LOCATION)
        .and_then(|to| to.to_str().ok())
        .unwrap_or_default();
    match leads_to(response.url(), to) {
        Some(id) => Ok(id),
        // Dead ones lead to TikTok's home page.
        None => bail!("the short link leads to no video (it may have expired, or be a photo post)"),
    }
}

/// The video a short link's redirect to `to` leads to, `to` read against
/// the link `from` (it may be relative): only a video's link counts.
fn leads_to(from: &reqwest::Url, to: &str) -> Option<VideoId> {
    match parse(from.join(to).ok()?.as_str())? {
        Link::Video(id) => Some(id),
        Link::Short(_) => None,
    }
}

/// Video `id`, read from its page's `__UNIVERSAL_DATA_FOR_REHYDRATION__`
/// (its `webapp.video-detail`), or why it isn't there.
fn read_page(page: &str, id: &VideoId) -> Result<Video> {
    let Some(data) = universal_data(page) else {
        // yt-dlp answers it (a proof of work); none came while measuring.
        if page.contains("id=\"cs\"") {
            bail!("TikTok sent a challenge instead of the page");
        }
        bail!("its page has no video");
    };
    let data: Value = serde_json::from_str(data).context("its page's data")?;
    let detail = &data["__DEFAULT_SCOPE__"]["webapp.video-detail"];
    // yt-dlp's numbers.
    match video::number(detail.get("statusCode")) {
        Some(0) => {}
        Some(10216) => bail!("the video is private"),
        Some(10222) => bail!("the account is private"),
        Some(10204) => bail!("TikTok won't show it here (blocked for this IP)"),
        Some(code) => bail!("TikTok has no video there (status {code})"),
        None => bail!("its page has no video"),
    }
    let item = &detail["itemInfo"]["itemStruct"];
    if item.get("imagePost").is_some() {
        bail!("it's a photo post: tuitok plays videos");
    }
    let Some(video) = Video::parse(item) else {
        if item.get("isContentClassified").and_then(Value::as_bool) == Some(true) {
            bail!("TikTok shows it only to accounts (age-restricted)");
        }
        bail!("its page has nothing that plays");
    };
    if &video.id != id {
        bail!("its page has another video");
    }
    Ok(video)
}

/// What the page's `__UNIVERSAL_DATA_FOR_REHYDRATION__` script holds.
fn universal_data(page: &str) -> Option<&str> {
    let tag = page.find("id=\"__UNIVERSAL_DATA_FOR_REHYDRATION__\"")?;
    let start = tag + page[tag..].find('>')? + 1;
    let end = start + page[start..].find("</script>")?;
    Some(&page[start..end])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID: &str = "7657182246486478098";

    fn video(id: &str) -> Option<Link> {
        Some(Link::Video(VideoId::new(id).unwrap()))
    }

    fn short(url: &str) -> Option<Link> {
        Some(Link::Short(url.into()))
    }

    #[test]
    fn video_links_of_every_kind() {
        for link in [
            "https://www.tiktok.com/@ngkhangg.008/video/7657182246486478098",
            "https://www.tiktok.com/@ngkhangg.008/video/7657182246486478098?is_from_webapp=1&sender_device=pc",
            "www.tiktok.com/@_/video/7657182246486478098/",
            "tiktok.com/@someone/video/7657182246486478098",
            "  https://WWW.TikTok.com/@x/video/7657182246486478098  ",
            "http://m.tiktok.com/v/7657182246486478098.html",
            "https://www.tiktok.com/embed/v2/7657182246486478098",
            "https://www.tiktok.com/embed/7657182246486478098",
            "https://www.tiktok.com/player/v1/7657182246486478098?autoplay=1",
            "7657182246486478098",
        ] {
            assert_eq!(parse(link), video(ID), "{link}");
        }
        assert_eq!(
            parse("https://vt.tiktok.com/ZSe4FqkKd"),
            short("https://vt.tiktok.com/ZSe4FqkKd/")
        );
        assert_eq!(
            parse("vm.tiktok.com/ZTR45GpSF/"),
            short("https://vm.tiktok.com/ZTR45GpSF/")
        );
        assert_eq!(
            parse("https://www.tiktok.com/t/ZTRC5xgJp?_r=1"),
            short("https://www.tiktok.com/t/ZTRC5xgJp/")
        );
        // Where a live short link led (`results/short-link.txt`).
        assert_eq!(
            parse(
                "https://www.tiktok.com/@anhtmebong/video/7691835404852399381?_r=1&_t=ZS-9AEQe2mkMUc"
            ),
            video("7691835404852399381")
        );
    }

    #[test]
    fn anything_else_is_not_a_link() {
        for text in [
            "",
            "hello",
            "open 7657182246486478098",
            "2024",
            "765718224648647809",
            "https://www.tiktok.com/",
            "https://www.tiktok.com/?_r=1",
            "https://www.tiktok.com/@someone",
            "https://www.tiktok.com/@someone/photo/7657182246486478098",
            "https://www.tiktok.com/someone/video/7657182246486478098",
            "https://www.tiktok.com/@someone/video/12ab",
            "https://www.tiktok.com/@someone/video/12345678901234567890123456",
            "https://tiktok.com.evil.com/@x/video/7657182246486478098",
            "https://evil.com/www.tiktok.com/@x/video/7657182246486478098",
            "https://user@www.tiktok.com/@x/video/7657182246486478098",
            "https://www.tiktok.com:8443/@x/video/7657182246486478098",
            "ftp://www.tiktok.com/@x/video/7657182246486478098",
            "javascript:alert(1)",
            "https://vt.tiktok.com/",
            "https://vt.tiktok.com/a%2Fb",
            "https://vt.tiktok.com/ZSe4FqkKd/more",
            "https://vt.tiktok.com/ZSe4FqkKdZSe4FqkKdZSe4FqkKdZSe4FqkKd",
            "https://lite.tiktok.com/t/ZSe4FqkKd",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
        let long = format!(
            "https://www.tiktok.com/@x/video/{ID}?q={}",
            "a".repeat(MAX_LINK)
        );
        assert_eq!(parse(&long), None);
    }

    #[test]
    fn a_short_link_leads_only_to_a_video_even_by_a_relative_address() {
        let from = reqwest::Url::parse("https://www.tiktok.com/t/ZTRC5xgJp/").unwrap();
        let id = |to: &str| leads_to(&from, to).map(|id| id.as_str().to_owned());
        assert_eq!(id("/@someone/video/123?_r=1").as_deref(), Some("123"));
        assert_eq!(
            id("https://www.tiktok.com/@someone/video/123").as_deref(),
            Some("123")
        );
        for nowhere in [
            "",
            "https://www.tiktok.com/?_r=1",
            "/@someone/photo/123",
            "https://vt.tiktok.com/ZSe4FqkKd/",
            "https://evil.com/@someone/video/123",
            "//evil.com/@someone/video/123",
        ] {
            assert_eq!(id(nowhere), None, "{nowhere}");
        }
    }

    #[test]
    fn only_the_id_is_fetched_on_tiktoks_own_host() {
        let id = VideoId::new(ID).unwrap();
        assert_eq!(
            page_url(&id),
            "https://www.tiktok.com/@_/video/7657182246486478098"
        );
        assert!(video::www_url_allowed(&page_url(&id)));
    }

    /// A video page holding `detail` as `webapp.video-detail`.
    fn page(detail: &Value) -> String {
        let data =
            json!({"__DEFAULT_SCOPE__": {"webapp.app-context": {}, "webapp.video-detail": detail}});
        format!(
            "<html><head><script id=\"__UNIVERSAL_DATA_FOR_REHYDRATION__\" type=\"application/json\">{data}</script>\
             <script>x</script></head></html>"
        )
    }

    fn item(id: &str) -> Value {
        json!({"id": id, "desc": "a", "author": {"uniqueId": "someone"},
               "video": {"playAddr": "https://v16-webapp-prime.tiktok.com/x", "duration": 11}})
    }

    #[test]
    fn a_page_s_video_is_read_as_the_feed_s() {
        let id = VideoId::new(ID).unwrap();
        let found = |detail: Value| read_page(&page(&detail), &id).map_err(|e| e.to_string());
        let ok = json!({"statusCode": 0, "itemInfo": {"itemStruct": item(ID)}});
        let video = found(ok).unwrap();
        assert_eq!((video.id.as_str(), video.seconds), (ID, 11));
        assert_eq!(video.author, "someone");
        let failed = |detail: Value| found(detail).unwrap_err();
        assert_eq!(failed(json!({"statusCode": 10216})), "the video is private");
        assert_eq!(
            failed(json!({"statusCode": 10222})),
            "the account is private"
        );
        assert_eq!(
            failed(json!({"statusCode": 10204})),
            "TikTok won't show it here (blocked for this IP)"
        );
        assert_eq!(
            failed(json!({"statusCode": "10101"})),
            "TikTok has no video there (status 10101)"
        );
        assert_eq!(failed(json!({})), "its page has no video");
        let mut photo = item(ID);
        photo["imagePost"] = json!({"images": []});
        assert_eq!(
            failed(json!({"statusCode": 0, "itemInfo": {"itemStruct": photo}})),
            "it's a photo post: tuitok plays videos"
        );
        let nothing = json!({"id": ID, "isContentClassified": false});
        assert_eq!(
            failed(json!({"statusCode": 0, "itemInfo": {"itemStruct": nothing}})),
            "its page has nothing that plays"
        );
        let restricted = json!({"id": ID, "isContentClassified": true});
        assert_eq!(
            failed(json!({"statusCode": 0, "itemInfo": {"itemStruct": restricted}})),
            "TikTok shows it only to accounts (age-restricted)"
        );
        assert_eq!(
            failed(json!({"statusCode": 0, "itemInfo": {"itemStruct": item("1")}})),
            "its page has another video"
        );
        let challenge = "<html><p id=\"cs\" class=\"eyJ2Ijp7fX0\"></p></html>";
        assert_eq!(
            read_page(challenge, &id).unwrap_err().to_string(),
            "TikTok sent a challenge instead of the page"
        );
        assert!(read_page("<html></html>", &id).is_err());
        let broken = "<script id=\"__UNIVERSAL_DATA_FOR_REHYDRATION__\">{</script>";
        assert!(read_page(broken, &id).is_err());
    }

    /// Against TikTok: a video of Explore's first page, opened by its link
    /// (`cargo test -- --ignored live`).
    #[tokio::test]
    #[ignore]
    async fn live_link() {
        let client = cache::client(video::www_url_allowed);
        let page = explore::fetch(&client, explore::CATEGORIES[2], None)
            .await
            .unwrap();
        let id = page.videos[0].id.clone();
        let link = format!("https://www.tiktok.com/@someone/video/{}", id.as_str());
        let short_client = cache::client(|_| false);
        let video = fetch(&client, &short_client, parse(&link).unwrap())
            .await
            .unwrap();
        assert_eq!(video.id, id);
        assert!(video.token.is_some() && !video.renditions.is_empty());
        // A dead short link (yt-dlp's) leads to TikTok's home page: its
        // redirect is read, not followed.
        let dead = parse("https://vm.tiktok.com/ZTR45GpSF/").unwrap();
        let why = fetch(&client, &short_client, dead)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            why,
            "the short link leads to no video (it may have expired, or be a photo post)"
        );
    }
}
