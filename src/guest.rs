//! What one run hands the next, so each start brings new videos instead of
//! the same popular ones again: the guest's TikTok cookies (the next run's
//! browser goes on as the same guest, so TikTok's feed goes on from where it
//! was) and the ids of the videos watched (skipped if TikTok sends one
//! again). `guest.json` in the data folder, readable only by you, written
//! whole when a run ends; `tuitok --forget` deletes it. Read back as
//! untrusted as anything from TikTok: the cookies are checked again as when
//! they came (`feed::guest_cookies`), expired ones dropped, the ids checked
//! as ids, the file's size capped.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::feed;
use crate::tools::shown;
use crate::video::VideoId;

const FILE: &str = "guest.json";
/// The most watched ids kept, the newest: days of scrolling.
const MAX_WATCHED: usize = 5_000;
/// The biggest file read back: 5,000 ids and the cookies are about 130 KB.
const MAX_BYTES: u64 = 1024 * 1024;

/// The guest one run hands the next.
#[derive(Debug, Default, PartialEq)]
pub struct Guest {
    /// As `Network.setCookies` takes them.
    pub cookies: Vec<Value>,
    /// Oldest first.
    pub watched: Vec<VideoId>,
}

fn path(data: &Path) -> PathBuf {
    data.join(FILE)
}

/// The last run's guest: none if there's no file, or one that isn't a
/// plain file of yours, is too big or doesn't read.
pub fn load(data: &Path) -> Guest {
    read(&path(data)).unwrap_or_default()
}

fn read(path: &Path) -> Option<Guest> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.uid() != crate::config::euid() || meta.len() > MAX_BYTES {
        return None;
    }
    let json: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    Some(parse(&json, now))
}

/// A guest from the file's JSON, checked again; cookies that expired by
/// `now` (seconds since 1970) are dropped.
fn parse(json: &Value, now: f64) -> Guest {
    let cookies = feed::guest_cookies(&json!({ "cookies": json["cookies"] }))
        .into_iter()
        .filter(|c| c["expires"].as_f64().is_none_or(|at| at > now))
        .collect();
    let mut watched: Vec<VideoId> = json["watched"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| VideoId::new(id.as_str()?))
        .collect();
    watched.drain(..watched.len().saturating_sub(MAX_WATCHED));
    Guest { cookies, watched }
}

/// Keeps `guest` for the next run, written whole
/// (`config::write_private`), so a crash halfway leaves the old one.
pub fn save(data: &Path, guest: &Guest) -> Result<()> {
    let newest = &guest.watched[guest.watched.len().saturating_sub(MAX_WATCHED)..];
    let json = json!({
        "cookies": guest.cookies,
        "watched": newest.iter().map(VideoId::as_str).collect::<Vec<_>>(),
    });
    crate::config::write_private(&path(data), json.to_string().as_bytes())
}

/// `tuitok --forget`: deletes the guest; whether there was one.
pub fn forget(data: &Path) -> Result<bool> {
    let target = path(data);
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot delete {}", shown(&target))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> VideoId {
        VideoId::new(&n.to_string()).unwrap()
    }

    #[test]
    fn what_comes_back_is_checked_again() {
        let file = json!({
            "cookies": [
                {"name": "ttwid", "value": "1%7Cab", "domain": ".tiktok.com", "expires": 2.0e9},
                {"name": "old", "value": "x", "domain": ".tiktok.com", "expires": 1.0e9},
                {"name": "session", "value": "y", "domain": "www.tiktok.com"},
                {"name": "evil", "value": "z", "domain": "evil.com"},
                {"name": "bad", "value": "a;b", "domain": ".tiktok.com"},
            ],
            "watched": ["1", "x", "12345678901234567890123456", 2, "3"],
        });
        let guest = parse(&file, 1.5e9);
        let names: Vec<&str> = guest
            .cookies
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert_eq!(names, ["ttwid", "session"]);
        assert_eq!(guest.watched, [id(1), id(3)]);
        assert_eq!(parse(&json!("nonsense"), 0.0), Guest::default());
        let many: Vec<String> = (1..=MAX_WATCHED as u64 + 10)
            .map(|n| n.to_string())
            .collect();
        let kept = parse(&json!({ "watched": many }), 0.0).watched;
        assert_eq!((kept.len(), kept.first()), (MAX_WATCHED, Some(&id(11))));
    }

    #[test]
    fn kept_only_for_you_and_forgotten() {
        use std::os::unix::fs::PermissionsExt;
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/test-guest-{}", std::process::id()));
        crate::config::private_dir(&data).unwrap();
        assert_eq!(load(&data), Guest::default());
        let guest = Guest {
            cookies: vec![
                json!({"name": "ttwid", "value": "a", "domain": ".tiktok.com", "path": "/", "secure": true, "httpOnly": true}),
            ],
            watched: vec![id(7), id(8)],
        };
        save(&data, &guest).unwrap();
        let mode = std::fs::metadata(path(&data)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(load(&data), guest);
        // Saved again over it, and nothing half-written left about.
        save(&data, &Guest::default()).unwrap();
        assert_eq!(load(&data), Guest::default());
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 1);
        assert!(forget(&data).unwrap());
        assert!(!forget(&data).unwrap());
        // A link in its place isn't followed.
        std::os::unix::fs::symlink("/etc/hosts", path(&data)).unwrap();
        assert_eq!(load(&data), Guest::default());
        std::fs::remove_dir_all(&data).unwrap();
    }
}
