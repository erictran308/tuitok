//! A video of the feed, read leniently from TikTok's `itemList` JSON: every
//! field is optional, and an item without a checked id or a playable
//! rendition is dropped. Text is cleaned and capped where it's read, and
//! only addresses on TikTok's own hosts are kept.

use serde_json::Value;

use crate::text;

/// The longest caption kept (TikTok's own limit is 4,000).
pub const MAX_CAPTION: usize = 4_000;
/// The longest name or handle kept.
pub const MAX_NAME: usize = 64;
/// The longest song name kept.
pub const MAX_MUSIC: usize = 120;
/// Renditions kept per video, addresses per rendition, and an address's
/// length.
const MAX_RENDITIONS: usize = 16;
const MAX_URLS: usize = 4;
const MAX_URL: usize = 4_096;

/// A TikTok video's id: 1–25 digits.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VideoId(String);

impl VideoId {
    pub fn new(id: &str) -> Option<Self> {
        ((1..=25).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit()))
            .then(|| Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
        }
    }
}

/// One encoding of a video: TikTok offers several (`video.bitrateInfo`).
#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub bytes: Option<u64>,
    /// Its addresses, each on TikTok's hosts ([`video_url_allowed`]).
    pub urls: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Video {
    pub id: VideoId,
    /// The author's handle, without the @.
    pub author: String,
    /// The author's display name.
    pub name: String,
    pub caption: String,
    pub music: String,
    pub seconds: u32,
    pub plays: Option<u64>,
    pub likes: Option<u64>,
    pub comments: Option<u64>,
    pub shares: Option<u64>,
    /// The cover picture, on TikTok's image hosts ([`image_url_allowed`]).
    pub cover: Option<String>,
    pub renditions: Vec<Rendition>,
}

impl Video {
    /// The video in one item of `itemList`, if it has a checked id and a
    /// rendition to play.
    pub fn parse(item: &Value) -> Option<Self> {
        let id = VideoId::new(item.get("id")?.as_str()?)?;
        let video = item.get("video")?;
        let mut renditions: Vec<Rendition> = video
            .get("bitrateInfo")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(rendition)
            .take(MAX_RENDITIONS)
            .collect();
        if renditions.is_empty() {
            // Only the default address: H.264, the video's own size.
            let url = video.get("playAddr").and_then(Value::as_str)?;
            renditions.push(Rendition {
                codec: Codec::H264,
                width: number(video.get("width")).unwrap_or(0) as u32,
                height: number(video.get("height")).unwrap_or(0) as u32,
                bytes: None,
                urls: vec![url.to_owned()],
            });
            renditions.retain(|r| r.urls.iter().all(|u| video_url_allowed(u)));
            if renditions.is_empty() {
                return None;
            }
        }
        let field = |path: &[&str]| path.iter().try_fold(item, |v, key| v.get(key));
        let string = |path: &[&str]| field(path).and_then(Value::as_str).unwrap_or("");
        let stat = |name: &str| {
            number(field(&["stats", name])).or_else(|| number(field(&["statsV2", name])))
        };
        let music = [
            string(&["music", "title"]),
            string(&["music", "authorName"]),
        ]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        Some(Self {
            id,
            author: text::one_line(string(&["author", "uniqueId"]), MAX_NAME),
            name: text::one_line(string(&["author", "nickname"]), MAX_NAME),
            caption: text::lines(string(&["desc"]), MAX_CAPTION),
            music: text::one_line(&music, MAX_MUSIC),
            seconds: number(video.get("duration")).unwrap_or(0).min(24 * 3600) as u32,
            plays: stat("playCount"),
            likes: stat("diggCount"),
            comments: stat("commentCount"),
            shares: stat("shareCount"),
            cover: ["cover", "originCover"]
                .iter()
                .filter_map(|key| video.get(*key).and_then(Value::as_str))
                .find(|url| image_url_allowed(url))
                .map(str::to_owned),
            renditions,
        })
    }

    /// The rendition to play in a box of `width`×`height` pixels: H.265
    /// where there is one (sharper and smaller than TikTok's default H.264),
    /// the smallest (in pixels, then bytes) that fills the box's width or
    /// its height at its own size, give or take a tenth, else the largest.
    /// A landscape video in an upright box fills only its width, so a
    /// smaller rendition does.
    pub fn best(&self, (width, height): (u32, u32)) -> Option<&Rendition> {
        let fills = |r: &Rendition| r.width * 10 >= width * 9 || r.height * 10 >= height * 9;
        let pixels = |r: &Rendition| u64::from(r.width) * u64::from(r.height);
        let of = |codec: Codec| {
            let all = || self.renditions.iter().filter(move |r| r.codec == codec);
            all()
                .filter(|r| fills(r))
                .min_by_key(|r| (pixels(r), r.bytes.unwrap_or(u64::MAX)))
                .or_else(|| all().max_by_key(|r| (pixels(r), std::cmp::Reverse(r.bytes))))
        };
        of(Codec::H265).or_else(|| of(Codec::H264))
    }
}

/// One entry of `bitrateInfo`, if its codec plays everywhere (H.264, H.265;
/// not ByteDance's own `bytevc2`) and it has an address on TikTok's hosts.
fn rendition(entry: &Value) -> Option<Rendition> {
    let codec = match entry.get("CodecType")?.as_str()? {
        "h264" => Codec::H264,
        kind if kind.starts_with("h265") => Codec::H265,
        _ => return None,
    };
    let play = entry.get("PlayAddr")?;
    let urls: Vec<String> = play
        .get("UrlList")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|url| video_url_allowed(url))
        .take(MAX_URLS)
        .map(str::to_owned)
        .collect();
    (!urls.is_empty()).then(|| Rendition {
        codec,
        width: number(play.get("Width")).unwrap_or(0).min(16_384) as u32,
        height: number(play.get("Height")).unwrap_or(0).min(16_384) as u32,
        bytes: number(play.get("DataSize")),
        urls,
    })
}

/// A count, as a number or a string of digits.
fn number(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// The host of an `https://` address with a plain one: lowercase letters,
/// digits, dots and hyphens; no user, password, port or escapes. The whole
/// address must be printable ASCII without spaces, and not too long.
fn https_host(url: &str) -> Option<&str> {
    if url.len() > MAX_URL || !url.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let rest = url.strip_prefix("https://")?;
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let plain = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-';
    (!host.is_empty() && host.bytes().all(plain)).then_some(host)
}

/// A video address tuitok fetches: https on TikTok's video hosts
/// (`v16-webapp-prime.tiktok.com` and the like), not `www.tiktok.com`,
/// whose addresses yt-dlp found broken.
pub fn video_url_allowed(url: &str) -> bool {
    https_host(url).is_some_and(|host| host.ends_with(".tiktok.com") && host != "www.tiktok.com")
}

/// A picture address tuitok fetches: https on TikTok's image hosts.
pub fn image_url_allowed(url: &str) -> bool {
    https_host(url).is_some_and(|host| {
        [".tiktokcdn.com", ".tiktokcdn-us.com", ".tiktokcdn-eu.com"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const V16: &str = "https://v16-webapp-prime.tiktok.com/video/tos/a/b/?mime_type=video_mp4";

    fn entry(codec: &str, height: u32, bytes: u64) -> Value {
        json!({"CodecType": codec, "GearName": "x", "PlayAddr": {
            "Width": height * 9 / 16, "Height": height, "DataSize": bytes,
            "UrlList": [V16, "https://www.tiktok.com/aweme/v1/play/?x=1"]}})
    }

    fn item() -> Value {
        json!({
            "id": "7658861391901740308",
            "desc": "  line one\u{1b}[2J \n\n\n line two ",
            "author": {"uniqueId": "someone", "nickname": "Some\u{202E}one"},
            "music": {"title": "original sound", "authorName": "someone"},
            "stats": {"playCount": 1234, "diggCount": 56},
            "statsV2": {"commentCount": "7", "shareCount": "8"},
            "video": {
                "duration": 12,
                "playAddr": V16,
                "cover": "https://p16-common-sign.tiktokcdn.com/x.jpeg",
                "bitrateInfo": [
                    entry("h264", 1024, 2_521_170),
                    entry("h265_hvc1", 1920, 1_481_543),
                    entry("h265_hvc1", 1280, 779_704),
                    entry("h265_hvc1", 1024, 642_961),
                    entry("h265_hvc1", 1024, 431_361),
                    entry("bytevc2", 1920, 100),
                ]
            }
        })
    }

    #[test]
    fn an_item_is_read_cleaned_and_its_addresses_checked() {
        let video = Video::parse(&item()).unwrap();
        assert_eq!(video.id.as_str(), "7658861391901740308");
        assert_eq!(video.author, "someone");
        assert_eq!(video.name, "Someone");
        assert_eq!(video.caption, "line one[2J\n\nline two");
        assert_eq!(video.music, "original sound · someone");
        assert_eq!(
            (video.seconds, video.plays, video.likes),
            (12, Some(1234), Some(56))
        );
        assert_eq!((video.comments, video.shares), (Some(7), Some(8)));
        assert!(video.cover.is_some());
        // bytevc2 dropped, www.tiktok.com addresses dropped.
        assert_eq!(video.renditions.len(), 5);
        assert!(video.renditions.iter().all(|r| r.urls == [V16]));
    }

    #[test]
    fn the_best_rendition_is_the_smallest_h265_that_fills_the_box() {
        let video = Video::parse(&item()).unwrap();
        let best = |b| video.best(b).map(|r| (r.codec, r.height, r.bytes.unwrap()));
        assert_eq!(best((562, 1000)), Some((Codec::H265, 1024, 431_361)));
        assert_eq!(best((675, 1200)), Some((Codec::H265, 1280, 779_704)));
        // Bigger than any: the biggest.
        assert_eq!(best((2000, 4000)), Some((Codec::H265, 1920, 1_481_543)));
        // A landscape video in an upright box needs only the box's width.
        let mut wide = item();
        wide["video"]["bitrateInfo"] = json!([
            {"CodecType": "h265_hvc1", "PlayAddr": {"Width": 1920, "Height": 1080, "UrlList": [V16]}},
            {"CodecType": "h265_hvc1", "PlayAddr": {"Width": 960, "Height": 540, "UrlList": [V16]}},
        ]);
        let wide = Video::parse(&wide).unwrap();
        assert_eq!(wide.best((608, 1080)).map(|r| r.width), Some(960));
    }

    #[test]
    fn items_without_a_good_id_or_a_playable_address_are_dropped() {
        let mut bad = item();
        bad["id"] = json!("12ab");
        assert!(Video::parse(&bad).is_none());
        let mut bare = item();
        bare["video"]["bitrateInfo"] = json!([]);
        bare["video"]["playAddr"] = json!("http://v16-webapp-prime.tiktok.com/x");
        assert!(Video::parse(&bare).is_none());
        bare["video"]["playAddr"] = json!(V16);
        let video = Video::parse(&bare).unwrap();
        assert_eq!(video.renditions[0].codec, Codec::H264);
    }

    #[test]
    fn only_tiktoks_hosts_pass() {
        assert!(video_url_allowed(V16));
        assert!(!video_url_allowed("https://www.tiktok.com/aweme/v1/play/"));
        assert!(!video_url_allowed("https://tiktok.com.evil.com/x"));
        assert!(!video_url_allowed("https://evil.com/.tiktok.com"));
        assert!(!video_url_allowed(
            "https://user@v16-webapp-prime.tiktok.com/x"
        ));
        assert!(!video_url_allowed(
            "https://v16-webapp-prime.tiktok.com:8443/x"
        ));
        assert!(!video_url_allowed(
            "https://v16-webapp-prime.tiktok.com/a b"
        ));
        assert!(!video_url_allowed("https://V16-webapp-prime.tiktok.com/x"));
        assert!(image_url_allowed(
            "https://p16-common-sign.tiktokcdn.com/x.jpeg?a=1"
        ));
        assert!(!image_url_allowed(
            "https://p16-common-sign.tiktokcdn.com.evil.com/x"
        ));
        assert!(!image_url_allowed(V16));
    }
}
