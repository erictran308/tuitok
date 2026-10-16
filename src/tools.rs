//! The programs tuitok runs, each found once as an absolute path: from its
//! `TUITOK_*` variable, else `PATH`'s absolute folders, then Homebrew's. A
//! program is never started by bare name, so none can come from the folder
//! tuitok was started in.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Programs start one at a time, and a pipe a child is to inherit is made
/// under the same lock: macOS has no `pipe2`, so a program started by
/// another thread between making a pipe and marking it close-on-exec would
/// get it too.
static STARTING: Mutex<()> = Mutex::new(());

pub fn starting() -> MutexGuard<'static, ()> {
    STARTING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Variables passed on to a child: what a program needs to find its home,
/// its temporary folder, its language and time zone, and the system's
/// certificates. Nothing else of tuitok's environment.
const KEPT: [&str; 10] = [
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// The environment a child starts with: [`KEPT`] and a `PATH` of the
/// system's folders.
pub fn child_env() -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = KEPT
        .iter()
        .filter_map(|&key| Some((key.into(), std::env::var_os(key)?)))
        .collect();
    env.push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
    env
}

/// Where Homebrew puts programs, for a shell whose `PATH` lacks them.
const HOMEBREW: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// Where Chrome and Chromium are installed on macOS.
const CHROMES: [&str; 2] = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];

pub struct Tools {
    /// Pepper's player: `TUITOK_PEPPER_PLAY`, else `PEPPER_PLAY`, which
    /// Pepper gives every shell.
    pub pepper_play: Option<PathBuf>,
    /// The light headless browser for the feed (`brew install
    /// lightpanda-io/browser/lightpanda`).
    pub lightpanda: Option<PathBuf>,
    /// The fallback browser for the feed.
    pub chrome: Option<PathBuf>,
    /// The player outside Pepper.
    pub mpv: Option<PathBuf>,
}

impl Tools {
    pub fn find() -> Self {
        Self {
            pepper_play: from_var("TUITOK_PEPPER_PLAY").or_else(|| from_var("PEPPER_PLAY")),
            lightpanda: from_var("TUITOK_LIGHTPANDA").or_else(|| on_path("lightpanda")),
            chrome: from_var("TUITOK_CHROME")
                .or_else(|| CHROMES.iter().map(PathBuf::from).find(|p| runnable(p))),
            mpv: from_var("TUITOK_MPV").or_else(|| on_path("mpv")),
        }
    }
}

fn from_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && runnable(p))
}

pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|folder| folder.is_absolute())
        .chain(HOMEBREW.iter().map(PathBuf::from))
        .map(|folder| folder.join(name))
        .find(|p| runnable(p))
}

fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `path` with only its absolute folders.
pub fn absolute_path(path: &OsStr) -> OsString {
    std::env::join_paths(std::env::split_paths(path).filter(|folder| folder.is_absolute()))
        .unwrap_or_default()
}

/// A path as it can be printed: characters that don't show (escape
/// sequences, line breaks) as `?`.
pub fn shown(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_folders_are_dropped() {
        assert_eq!(
            absolute_path(OsStr::new("/usr/bin:bin:.::/opt/homebrew/bin")),
            OsString::from("/usr/bin:/opt/homebrew/bin")
        );
    }

    #[test]
    fn paths_are_printed_without_escape_sequences() {
        assert_eq!(shown(Path::new("/a/\x1b]0;x\x07b")), "/a/?]0;x?b");
    }
}
