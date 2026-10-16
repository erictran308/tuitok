//! A video's top comments, in TikTok's own order. Its web page asks
//! `www.tiktok.com/api/comment/list/` for them, and unlike the feed that
//! answers a plain request: no signature, no cookie (tried on 12 videos, 0.4
//! to 0.7 s and about 15 KB each). The address is made from a checked id
//! only; the answer is read leniently, its text cleaned and capped, and kept
//! in memory for the videos near the one shown, never on disk.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::cache;
use crate::text;
use crate::video::{self, MAX_NAME, VideoId};

/// Comments asked for: TikTok sent 3 to 5 of 5, holding some back.
pub const COUNT: usize = 5;
/// The biggest answer read: 5 comments came to about 15 KB.
const MAX_BYTES: usize = 512 * 1024;
/// The longest comment kept: TikTok's own limit is 150 characters.
const MAX_TEXT: usize = 300;
/// How long a fetch may take.
const WAIT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    /// Who wrote it: their name, else their handle.
    pub name: String,
    pub text: String,
    pub likes: Option<u64>,
}

/// A video's comments, as far as they've got.
#[derive(Debug, PartialEq)]
pub enum Comments {
    Loading,
    Ready(Vec<Comment>),
    Failed,
}

/// Where video `id`'s top comments come from.
pub fn url(id: &VideoId) -> String {
    format!(
        "https://www.tiktok.com/api/comment/list/?aid=1988&aweme_id={}&count={COUNT}&cursor=0",
        id.as_str()
    )
}

/// Whether `url` may be followed to for comments: https on www.tiktok.com.
pub fn url_allowed(url: &str) -> bool {
    video::https_host(url) == Some("www.tiktok.com")
}

/// Video `id`'s top comments, off the event loop.
pub async fn fetch(client: &reqwest::Client, id: &VideoId) -> Result<Vec<Comment>> {
    let mut response = client
        .get(url(id))
        .header(reqwest::header::USER_AGENT, cache::USER_AGENT)
        .header(reqwest::header::REFERER, cache::REFERER)
        .timeout(WAIT)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("HTTP {}", response.status().as_u16());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if data.len() + chunk.len() > MAX_BYTES {
            bail!("the answer is too big");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(parse(&serde_json::from_slice(&data)?))
}

/// The comments in an answer: those with text (not only a sticker), at
/// most `COUNT`.
fn parse(answer: &Value) -> Vec<Comment> {
    answer["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|comment| {
            let text = text::lines(comment["text"].as_str()?, MAX_TEXT);
            if text.is_empty() {
                return None;
            }
            let user = &comment["user"];
            let name = ["nickname", "unique_id"]
                .iter()
                .map(|key| text::one_line(user[*key].as_str().unwrap_or(""), MAX_NAME))
                .find(|name| !name.is_empty())
                .unwrap_or_else(|| "someone".to_owned());
            Some(Comment {
                name,
                text,
                likes: video::number(comment.get("digg_count")),
            })
        })
        .take(COUNT)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_www_tiktok_com() {
        let id = VideoId::new("7658861391901740308").unwrap();
        assert_eq!(
            url(&id),
            "https://www.tiktok.com/api/comment/list/?aid=1988&aweme_id=7658861391901740308&count=5&cursor=0"
        );
        assert!(url_allowed(&url(&id)));
        assert!(!url_allowed("http://www.tiktok.com/api/comment/list/"));
        assert!(!url_allowed("https://www.tiktok.com.evil.com/"));
        assert!(!url_allowed("https://v16-webapp-prime.tiktok.com/x"));
    }

    #[test]
    fn answers_are_read_leniently_and_cleaned() {
        let answer = json!({"status_code": 0, "total": 2411, "comments": [
            {"text": "trời cây\u{1b}[2J chôm chôm", "digg_count": 866, "user": {"nickname": "Linh", "unique_id": "linh22245"}},
            {"text": "no name", "digg_count": "12", "user": {"nickname": "  ", "unique_id": "handle.only"}},
            {"text": "", "user": {"nickname": "sticker"}},
            {"user": {"nickname": "nothing"}},
            {"text": "x".repeat(1000), "user": {}},
            {"text": "6"}, {"text": "7"}, {"text": "8"},
        ]});
        let comments = parse(&answer);
        assert_eq!(comments.len(), COUNT);
        assert_eq!(
            comments[0],
            Comment {
                name: "Linh".into(),
                text: "trời cây[2J chôm chôm".into(),
                likes: Some(866),
            }
        );
        assert_eq!(
            (comments[1].name.as_str(), comments[1].likes),
            ("handle.only", Some(12))
        );
        assert_eq!(comments[2].name, "someone");
        assert_eq!(comments[2].text.chars().count(), MAX_TEXT);
        assert!(parse(&json!({"status_code": 0})).is_empty());
        assert!(parse(&json!("nonsense")).is_empty());
    }

    /// Against TikTok: a batch's first videos' comments
    /// (`cargo test -- --ignored live`).
    #[tokio::test]
    #[ignore]
    async fn live_comments() {
        let tools = crate::tools::Tools::find();
        let (engine, program) = crate::feed::engines(&tools, None).remove(0);
        let place =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/live-comments");
        let ask = crate::feed::Ask {
            want: 3,
            ..Default::default()
        };
        let batch = crate::feed::fetch(engine, &program, &place.join("chrome"), &ask, &|_| {})
            .await
            .unwrap();
        let client = cache::client(url_allowed);
        let mut found = 0;
        for video in &batch.videos {
            found += fetch(&client, &video.id).await.unwrap().len();
        }
        assert!(found > 0, "no comments on {} videos", batch.videos.len());
        let _ = std::fs::remove_dir_all(place);
    }
}
