//! Where tuitok keeps what it keeps (the Chrome profile now, the video cache
//! next): `TUITOK_DATA_DIR`, else the platform's place for app data
//! (`~/Library/Application Support/tuitok` on macOS,
//! `~/.local/share/tuitok` on Linux), made readable only by you, as
//! tuitube's `config.rs` does.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::tools::shown;

/// The data folder, made or checked so only you can reach it.
pub fn data_dir() -> Result<PathBuf> {
    make_private_dir(&data_dir_from(std::env::var("TUITOK_DATA_DIR").ok())?)
}

fn data_dir_from(set: Option<String>) -> Result<PathBuf> {
    if let Some(dir) = set {
        let dir = PathBuf::from(dir);
        // A relative path would depend on where tuitok is started: a cloned
        // folder could hand you its own data.
        if !dir.is_absolute() {
            bail!("TUITOK_DATA_DIR must be a full path to a folder on this computer");
        }
        return Ok(dir);
    }
    let base = dirs::data_local_dir().context("no home folder found; set TUITOK_DATA_DIR")?;
    Ok(base.join("tuitok"))
}

/// Makes the data folder, or checks the one that's there, so only you can
/// reach it, and gives its path with links resolved. In this order, so
/// nothing is created or changed somewhere unsafe: the folders above it may
/// be changed only by you or the system ([`private_place`]); it's then
/// made, mode 0700, or found to be a real folder (not a link) that's yours;
/// then set to 0700 again, and on macOS cleared of access rules inherited
/// from above, which would let others in whatever the mode says.
fn make_private_dir(dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let shown = shown(dir);
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        bail!("{shown} isn't a folder tuitok can use");
    };
    std::fs::create_dir_all(parent).with_context(|| format!("cannot create {shown}"))?;
    let parent = std::fs::canonicalize(parent).with_context(|| format!("cannot read {shown}"))?;
    private_place(&parent)?;
    let dir = parent.join(name);
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(e).with_context(|| format!("cannot create {shown}"));
        }
        _ => {}
    }
    let meta = std::fs::symlink_metadata(&dir).with_context(|| format!("cannot read {shown}"))?;
    if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != euid() {
        bail!("{shown} isn't a folder of yours, so it's no place for tuitok's data");
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot protect {shown}"))?;
    #[cfg(target_os = "macos")]
    clear_acls(&dir);
    Ok(dir)
}

/// Removes access rules (ACLs) from `dir` and everything in it: a folder
/// made inside one with an inherited rule for everyone gets that rule too,
/// and mode 0700 doesn't override it. `/bin/chmod -N` by its full path.
#[cfg(target_os = "macos")]
fn clear_acls(dir: &Path) {
    let _ = std::process::Command::new("/bin/chmod")
        .args(["-R", "-N"])
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The user tuitok runs as.
fn euid() -> u32 {
    // SAFETY: geteuid can't fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// Checks that only you or the system can change `dir` and the folders
/// above it: otherwise another account could rename the data folder away
/// and put its own in its place. Folders anyone may write to, like `/tmp`,
/// are fine when only an entry's owner can move it (the sticky bit) and the
/// system owns them.
fn private_place(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let uid = euid();
    // SAFETY: getgid can't fail and touches no memory.
    let gid = unsafe { libc::getgid() };
    for folder in std::fs::canonicalize(dir)?.ancestors() {
        let meta = std::fs::metadata(folder)?;
        let mode = meta.mode();
        // Your own group is only yours on Linux (user private groups); on
        // macOS every account is in `staff`.
        let group_is_others = cfg!(target_os = "macos") || meta.gid() != gid;
        let others_write = mode & 0o002 != 0 || (mode & 0o020 != 0 && group_is_others);
        let sticky = mode & 0o1000 != 0;
        let owner_ok = meta.uid() == uid || meta.uid() == 0;
        if !owner_ok || (others_write && !(sticky && meta.uid() == 0)) {
            bail!(
                "{} can be changed by other users, so it's no place for tuitok's data; \
                 set TUITOK_DATA_DIR somewhere in your home folder",
                shown(folder)
            );
        }
    }
    Ok(())
}

/// The folders one run of tuitok uses, in `session-<pid>` in the data
/// folder, so two at once don't touch each other's: Chrome's profile and
/// the videos downloaded ahead. Deleted when the run ends ([`Session::end`]),
/// and by the next run if it crashed (its process gone).
pub struct Session {
    pub dir: PathBuf,
    pub profile: PathBuf,
    pub cache: PathBuf,
}

impl Session {
    pub fn start() -> Result<Self> {
        let data = data_dir()?;
        remove_left_behind(&data);
        let dir = data.join(format!("session-{}", std::process::id()));
        let session = Self {
            profile: dir.join("chrome"),
            cache: dir.join("cache"),
            dir,
        };
        let _ = std::fs::remove_dir_all(&session.dir);
        private_dir(&session.dir)?;
        private_dir(&session.cache)?;
        Ok(session)
    }

    pub fn end(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Deletes the session folders of runs whose process is gone.
fn remove_left_behind(data: &Path) {
    let Ok(entries) = std::fs::read_dir(data) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("session-"))
            .and_then(|n| n.parse::<i32>().ok())
            .filter(|&pid| pid > 0)
        else {
            continue;
        };
        // SAFETY: signal 0 only asks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Creates `dir` (and its parents) readable only by you.
pub fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", shown(dir)))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot protect {}", shown(dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_folder_must_be_a_full_path() {
        assert!(data_dir_from(Some("relative/dir".into())).is_err());
        assert_eq!(
            data_dir_from(Some("/Users/me/tt".into())).unwrap(),
            PathBuf::from("/Users/me/tt")
        );
        assert!(data_dir_from(None).unwrap().ends_with("tuitok"));
    }
}
