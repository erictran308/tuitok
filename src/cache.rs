//! Videos downloaded ahead, so the next one plays at once: the rendition
//! that fills the box (`Video::best`), fetched with the guest cookie its
//! batch got, into the session's cache folder (`config::Session`: only you
//! can read it, and it's emptied when tuitok ends, so it keeps no record of
//! what you watched). A video over `DOWNLOAD_UP_TO` isn't downloaded but
//! streamed when it plays.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::AsyncWriteExt;

use crate::text;
use crate::video::{Codec, Rendition, VideoId, video_url_allowed};

/// Videos downloaded after the one shown, and kept before it.
pub const AHEAD: usize = 3;
pub const BEHIND: usize = 2;

/// Downloads at once.
pub const PARALLEL: usize = 2;

/// The biggest file downloaded ahead: most of TikTok's are under 3 MB as
/// H.265, but some videos are an hour long (500 MB seen in a batch).
pub const DOWNLOAD_UP_TO: u64 = 30_000_000;

/// How often a download says how far it's got.
const REPORT_EVERY: u64 = 256 * 1024;

/// What TikTok's video hosts are sent besides the guest cookie: Chrome's
/// user agent and TikTok as the referrer, as its own page sends (tested
/// with tokens from Lightpanda and from Chrome alike).
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";
const REFERER: &str = "https://www.tiktok.com/";

/// Where a video's download is at.
#[derive(Clone, Debug, PartialEq)]
pub enum Download {
    /// Bytes so far, of how many if TikTok said.
    Getting {
        got: u64,
        of: Option<u64>,
    },
    Ready(PathBuf),
    /// Too big to download ahead: it streams when it plays.
    Streams,
    Failed(String),
}

/// One video to download: its rendition, its batch's cookie, and where.
pub struct Job {
    pub id: VideoId,
    pub rendition: Rendition,
    pub token: Option<Arc<str>>,
    pub path: PathBuf,
}

/// The client for TikTok's video hosts: rustls with ring, https only, no
/// cookies kept but the one each download sends, no proxy from the
/// environment (the browser gets none either), and redirects only to
/// TikTok's video hosts, at most 2.
pub fn client() -> reqwest::Client {
    // ring does the cryptography; the first client sets it for the process.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .https_only(true)
        .referer(false)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 3 && video_url_allowed(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .expect("the TLS backend is built in")
}

/// The file a rendition is kept in, named from its checked id, codec and
/// size only.
pub fn file_name(id: &VideoId, rendition: &Rendition) -> String {
    let codec = match rendition.codec {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
    };
    format!(
        "{}-{codec}-{}x{}.mp4",
        id.as_str(),
        rendition.width,
        rendition.height
    )
}

/// Downloads `job`, trying its addresses in turn; `progress` hears how far
/// it got. Never an error: what happened is the `Download` returned.
pub async fn download(
    client: &reqwest::Client,
    job: &Job,
    progress: impl Fn(u64, Option<u64>),
) -> Download {
    let mut failed = String::from("no address");
    for url in &job.rendition.urls {
        match get(client, url, job, &progress).await {
            Ok(()) => return Download::Ready(job.path.clone()),
            Err(e) if e.is::<TooBig>() => return Download::Streams,
            Err(e) => failed = text::one_line(&format!("{e:#}"), 200),
        }
    }
    Download::Failed(failed)
}

/// A file bigger than `DOWNLOAD_UP_TO`.
#[derive(Debug)]
struct TooBig;

impl std::fmt::Display for TooBig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "over {} MB", DOWNLOAD_UP_TO / 1_000_000)
    }
}

impl std::error::Error for TooBig {}

/// One address, written to `<path>.part` (made 0600, never over a file
/// there) and renamed into place once whole and found to be an MP4.
async fn get(
    client: &reqwest::Client,
    url: &str,
    job: &Job,
    progress: &impl Fn(u64, Option<u64>),
) -> Result<()> {
    let mut request = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::REFERER, REFERER);
    if let Some(token) = &job.token {
        request = request.header(reqwest::header::COOKIE, format!("tt_chain_token={token}"));
    }
    let mut response = request.send().await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("HTTP {}", response.status().as_u16());
    }
    let of = response.content_length();
    if of.is_some_and(|bytes| bytes > DOWNLOAD_UP_TO) {
        return Err(TooBig.into());
    }
    let part = job.path.with_extension("part");
    let _ = tokio::fs::remove_file(&part).await;
    match write(&mut response, &part, of, progress).await {
        Ok(()) => Ok(tokio::fs::rename(&part, &job.path).await?),
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            Err(e)
        }
    }
}

async fn write(
    response: &mut reqwest::Response,
    part: &Path,
    of: Option<u64>,
    progress: &impl Fn(u64, Option<u64>),
) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(part)
        .await?;
    let (mut got, mut told) = (0u64, 0u64);
    let mut head = Vec::with_capacity(12);
    while let Some(chunk) = response.chunk().await? {
        got += chunk.len() as u64;
        if got > DOWNLOAD_UP_TO {
            return Err(TooBig.into());
        }
        if head.len() < 12 {
            head.extend_from_slice(&chunk[..chunk.len().min(12 - head.len())]);
            if head.len() == 12 && !is_mp4(&head) {
                bail!("not an MP4");
            }
        }
        file.write_all(&chunk).await?;
        if got - told >= REPORT_EVERY {
            progress(got, of);
            told = got;
        }
    }
    if !is_mp4(&head) {
        bail!("not an MP4");
    }
    file.flush().await?;
    progress(got, of);
    Ok(())
}

/// Whether a file starts as an MP4 does: with a box of type `ftyp`.
fn is_mp4(head: &[u8]) -> bool {
    head.len() >= 8 && &head[4..8] == b"ftyp"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_mp4s() {
        let id = VideoId::new("7658861391901740308").unwrap();
        let rendition = Rendition {
            codec: Codec::H265,
            width: 576,
            height: 1024,
            bytes: None,
            urls: Vec::new(),
        };
        assert_eq!(
            file_name(&id, &rendition),
            "7658861391901740308-h265-576x1024.mp4"
        );
        assert!(is_mp4(b"\0\0\0\x20ftypisom\0\0\x02\0"));
        assert!(!is_mp4(b"<html><head>"));
        assert!(!is_mp4(b"\0\0"));
    }

    /// Against TikTok: `cargo test -- --ignored live`.
    #[tokio::test]
    #[ignore]
    async fn live_download() {
        let tools = crate::tools::Tools::find();
        let place = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/live-cache");
        crate::config::private_dir(&place).unwrap();
        let (engine, program) = crate::feed::engines(&tools, None).remove(0);
        let batch = crate::feed::fetch(engine, &program, &place.join("chrome"), 10)
            .await
            .unwrap();
        let client = client();
        for video in batch.videos.iter().take(2) {
            let rendition = video.best((608, 1080)).unwrap().clone();
            let job = Job {
                path: place.join(file_name(&video.id, &rendition)),
                id: video.id.clone(),
                rendition,
                token: video.token.clone(),
            };
            match download(&client, &job, |_, _| {}).await {
                Download::Ready(path) => assert!(is_mp4(&std::fs::read(path).unwrap())),
                Download::Streams => {}
                other => panic!("{other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(place);
    }
}
