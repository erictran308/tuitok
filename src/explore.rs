//! Explore's categories: what a logged-out guest can pick instead of For
//! You. TikTok answers its keyword search, hashtag and creator pages with
//! nothing unless you're logged in (HTTP 200, no body, in Lightpanda and
//! Chrome alike), but its Explore page's videos come by category from a
//! plain request: no browser, no signature (measured 2026-10-17:
//! `reports/Search and links.md`).
//!
//! One `GET /explore` makes a guest: its `ttwid` cookie, which
//! `/api/explore/item_list/` needs (without it: `statusCode` 100001, no
//! videos), and its `tt_chain_token`, which the videos' addresses need. Then
//! each page is about 15 popular videos for the IP's country, new ones each
//! time (five in a row: 74 different), in 0.5 to 0.7 s. Both cookies stay in
//! memory.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::header::{COOKIE, REFERER, SET_COOKIE, USER_AGENT};
use serde_json::Value;

use crate::cache;
use crate::feed;
use crate::video::Video;

/// What the command line calls the feed tuitok starts with.
pub const FOR_YOU: &str = "For You";

/// One of Explore's categories: TikTok's number for it (`categoryType`) and
/// its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Category {
    pub id: u16,
    pub name: &'static str,
}

const fn category(id: u16, name: &'static str) -> Category {
    Category { id, name }
}

/// Explore's categories in TikTok's order (`webapp.explore.categoryList` on
/// its page).
pub const CATEGORIES: [Category; 21] = [
    category(120, "All"),
    category(119, "Singing & Dancing"),
    category(104, "Comedy"),
    category(112, "Sports"),
    category(100, "Anime & Comics"),
    category(107, "Relationship"),
    category(101, "Shows"),
    category(110, "Lipsync"),
    category(105, "Daily Life"),
    category(102, "Beauty Care"),
    category(103, "Games"),
    category(114, "Society"),
    category(109, "Outfit"),
    category(115, "Cars"),
    category(111, "Food"),
    category(113, "Animals"),
    category(106, "Family"),
    category(108, "Drama"),
    category(117, "Fitness & Health"),
    category(116, "Education"),
    category(118, "Technology"),
];

/// The page that makes a guest, and the referrer of its requests.
const PAGE: &str = "https://www.tiktok.com/explore";
/// Videos asked for a page: TikTok sent 13 to 16 of 16.
const COUNT: usize = 16;
/// The biggest answer read: 16 videos came to 140 to 240 KB.
const MAX_BYTES: usize = 2 * 1024 * 1024;
/// The longest a cookie's value is taken (`ttwid` was 127 bytes).
const MAX_COOKIE: usize = 512;
/// How long a request may take.
const WAIT: Duration = Duration::from_secs(15);

/// What the feed shown is called: For You (`None`), or its category's name.
pub fn name(category: Option<Category>) -> &'static str {
    category.map_or(FOR_YOU, |c| c.name)
}

/// What `/` offers for `typed`: For You (`None`) and the categories
/// whose names hold it, ignoring case, those that start with it first, then
/// those with a word that does; everything, in TikTok's order, while
/// nothing's typed.
pub fn matching(typed: &str) -> Vec<Option<Category>> {
    let typed = typed.trim().to_lowercase();
    let all = std::iter::once(None).chain(CATEGORIES.iter().copied().map(Some));
    let rank = |category: Option<Category>| {
        let name = name(category).to_lowercase();
        if name.starts_with(&typed) {
            Some(0)
        } else if name.split_whitespace().any(|word| word.starts_with(&typed)) {
            Some(1)
        } else {
            name.contains(&typed).then_some(2)
        }
    };
    let mut found: Vec<(u8, Option<Category>)> = all.filter_map(|c| Some((rank(c)?, c))).collect();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, c)| c).collect()
}

/// The guest Explore's pages are fetched as: `ttwid`, which they need, and
/// `tt_chain_token`, which their videos' addresses need.
#[derive(Clone)]
pub struct Guest {
    ttwid: String,
    pub token: Arc<str>,
}

#[cfg(test)]
impl Guest {
    /// A made-up guest whose videos get `token`.
    pub fn made_up(token: &str) -> Self {
        Self {
            ttwid: "1%7Cmade-up".into(),
            token: Arc::from(token),
        }
    }
}

/// One page of a category.
pub struct Page {
    pub videos: Vec<Video>,
    /// The guest it was fetched as, for the next page.
    pub guest: Guest,
    pub took: Duration,
}

/// Where a page of `category` comes from.
pub fn url(category: Category) -> String {
    format!(
        "https://www.tiktok.com/api/explore/item_list/?aid=1988&categoryType={}&count={COUNT}",
        category.id
    )
}

/// A page of `category`, as `guest` if there is one; as a new guest if
/// there isn't, or that one gets nothing.
pub async fn fetch(
    client: &reqwest::Client,
    category: Category,
    guest: Option<Guest>,
) -> Result<Page> {
    let started = Instant::now();
    if let Some(guest) = guest
        && let Ok(videos) = page(client, category, &guest).await
    {
        let took = started.elapsed();
        return Ok(Page {
            videos,
            guest,
            took,
        });
    }
    let guest = new_guest(client).await?;
    let videos = page(client, category, &guest).await?;
    Ok(Page {
        videos,
        guest,
        took: started.elapsed(),
    })
}

/// A new guest: the cookies `GET /explore` sets.
async fn new_guest(client: &reqwest::Client) -> Result<Guest> {
    let response = client
        .get(PAGE)
        .header(USER_AGENT, cache::USER_AGENT)
        .timeout(WAIT)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("Explore's page: HTTP {}", response.status().as_u16());
    }
    let cookie = |name: &str| {
        response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|header| header.to_str().ok())
            .find_map(|header| set_cookie(header, name))
    };
    Ok(Guest {
        ttwid: cookie("ttwid").context("TikTok made no guest (no ttwid)")?,
        token: Arc::from(
            cookie("tt_chain_token").context("TikTok made no guest (no tt_chain_token)")?,
        ),
    })
}

/// The value cookie `name` is set to in a `Set-Cookie` header, if it fits
/// in a `Cookie` header as it is.
fn set_cookie(header: &str, name: &str) -> Option<String> {
    let (key, rest) = header.split_once('=')?;
    let value = rest.split(';').next()?.trim();
    (key.trim() == name && feed::fits_header(value, MAX_COOKIE)).then(|| value.to_owned())
}

/// The videos of a page of `category`, each with `guest`'s token.
async fn page(client: &reqwest::Client, category: Category, guest: &Guest) -> Result<Vec<Video>> {
    let response = client
        .get(url(category))
        .header(USER_AGENT, cache::USER_AGENT)
        .header(REFERER, PAGE)
        .header(COOKIE, format!("ttwid={}", guest.ttwid))
        .timeout(WAIT)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("HTTP {}", response.status().as_u16());
    }
    let data = cache::read_capped(response, MAX_BYTES, "the answer").await?;
    if data.is_empty() {
        bail!("TikTok sent nothing");
    }
    let videos = parse(&serde_json::from_slice(&data)?, &guest.token);
    if videos.is_empty() {
        bail!("TikTok sent no videos");
    }
    Ok(videos)
}

/// The videos in a page's answer, each with `token`.
fn parse(answer: &Value, token: &Arc<str>) -> Vec<Video> {
    answer["itemList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Video::parse)
        .map(|mut video| {
            video.token = Some(token.clone());
            video
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::video;

    fn names(typed: &str) -> Vec<&'static str> {
        matching(typed).into_iter().map(name).collect()
    }

    #[test]
    fn the_bar_offers_for_you_then_the_categories_best_first() {
        assert_eq!(matching("").len(), 22);
        assert_eq!(names("")[..3], ["For You", "All", "Singing & Dancing"]);
        assert_eq!(
            names("f"),
            [
                "For You",
                "Food",
                "Family",
                "Fitness & Health",
                "Daily Life",
                "Outfit"
            ]
        );
        assert_eq!(names("  FO "), ["For You", "Food"]);
        assert_eq!(names("dan"), ["Singing & Dancing"]);
        assert_eq!(names("life"), ["Daily Life"]);
        assert_eq!(names("ear"), Vec::<&str>::new());
        assert_eq!(
            names("&"),
            ["Singing & Dancing", "Anime & Comics", "Fitness & Health"]
        );
        assert_eq!(names("you"), ["For You"]);
    }

    #[test]
    fn categories_are_tiktoks_numbers_each_once() {
        let mut ids: Vec<u16> = CATEGORIES.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (100..=120).collect::<Vec<u16>>());
        assert_eq!(
            url(CATEGORIES[2]),
            "https://www.tiktok.com/api/explore/item_list/?aid=1988&categoryType=104&count=16"
        );
        assert!(video::www_url_allowed(&url(CATEGORIES[0])) && video::www_url_allowed(PAGE));
    }

    #[test]
    fn the_guest_is_the_cookies_explore_sets() {
        let ttwid = "ttwid=1%7CabcDEF; Domain=.tiktok.com; Path=/; Expires=Tue, 12 Oct 2027 02:57:52 GMT; HttpOnly; Secure";
        assert_eq!(set_cookie(ttwid, "ttwid").as_deref(), Some("1%7CabcDEF"));
        assert_eq!(set_cookie(ttwid, "tt_chain_token"), None);
        assert_eq!(
            set_cookie("tt_chain_token=a+b/c==; path=/", "tt_chain_token").as_deref(),
            Some("a+b/c==")
        );
        assert_eq!(set_cookie("ttwid=; Path=/", "ttwid"), None);
        assert_eq!(set_cookie("ttwid=a\"b", "ttwid"), None);
        assert_eq!(set_cookie("ttwid", "ttwid"), None);
    }

    #[test]
    fn a_pages_videos_get_the_guests_token() {
        let item = |id: &str| json!({"id": id, "video": {"playAddr": "https://v16-webapp-prime.tiktok.com/x"}});
        let answer = json!({"statusCode": 0, "hasMore": true, "itemList": [item("1"), {"id": "x"}, item("2")]});
        let videos = parse(&answer, &Arc::from("tok"));
        assert_eq!(videos.len(), 2);
        assert!(videos.iter().all(|v| v.token.as_deref() == Some("tok")));
        assert!(parse(&json!({"statusCode": 100001}), &Arc::from("tok")).is_empty());
    }

    /// Against TikTok: two pages of a category, the second as the first's
    /// guest (`cargo test -- --ignored live`).
    #[tokio::test]
    #[ignore]
    async fn live_explore() {
        let client = cache::client(video::www_url_allowed);
        let first = fetch(&client, CATEGORIES[2], None).await.unwrap();
        assert!(first.videos.len() >= 8, "{}", first.videos.len());
        let second = fetch(&client, CATEGORIES[2], Some(first.guest.clone()))
            .await
            .unwrap();
        let new = second
            .videos
            .iter()
            .filter(|v| !first.videos.iter().any(|f| f.id == v.id))
            .count();
        assert!(new > 0, "the second page repeated the first");
        let video_client = cache::client(video::video_url_allowed);
        let rendition = first.videos[0].best((608, 1080)).unwrap();
        let response = video_client
            .get(&rendition.urls[0])
            .header(USER_AGENT, cache::USER_AGENT)
            .header(REFERER, cache::REFERER)
            .header(COOKIE, format!("tt_chain_token={}", first.guest.token))
            .header(reqwest::header::RANGE, "bytes=0-1023")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 206);
    }
}
