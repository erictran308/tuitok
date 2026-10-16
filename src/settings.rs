//! Your settings, in `settings.toml` in the data folder, as tuitube keeps
//! its own: the theme (picked in `?`'s settings) and the volume (`-` and `+`, kept
//! when tuitok ends). Read leniently, as
//! `guest.json` is (a file that isn't a plain file of yours, is too big or
//! doesn't read gives the defaults; a theme that isn't built in is the
//! default one), and saved one setting at a time, so whatever else the
//! file holds stays.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::theme;
use crate::tools::shown;

const FILE: &str = "settings.toml";
/// The biggest file read: a setting is a line of a few dozen bytes.
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, PartialEq)]
pub struct Settings {
    /// The theme's id, one of `theme::BUILT_IN`'s.
    pub theme: &'static str,
    /// How loud videos play, in percent: 0 to 100 (`-` and `+` change it).
    pub volume: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: theme::DEFAULT,
            volume: 100,
        }
    }
}

fn path(data: &Path) -> PathBuf {
    data.join(FILE)
}

/// The settings in the data folder `data`, else the defaults; each one
/// that isn't what it should be is its default.
pub fn load(data: &Path) -> Settings {
    let table = read(&path(data)).ok().flatten().unwrap_or_default();
    let theme = table.get("theme").and_then(toml::Value::as_str);
    let volume = table.get("volume").and_then(toml::Value::as_integer);
    Settings {
        theme: theme::id(theme.unwrap_or_default()),
        volume: volume.map_or(100, |v| v.clamp(0, 100) as u32),
    }
}

/// What the file holds, or nothing if there's none; an error if it isn't
/// a plain file of yours, is too big or isn't TOML.
fn read(path: &Path) -> Result<Option<toml::Table>> {
    use std::os::unix::fs::MetadataExt;
    let shown = shown(path);
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {shown}")),
    };
    if !meta.is_file() || meta.uid() != crate::config::euid() {
        bail!("{shown} isn't a plain file of yours");
    }
    if meta.len() > MAX_BYTES {
        bail!("{shown} is too big for settings");
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {shown}"))?;
    // Not the parser's own message: it quotes the file.
    match toml::from_str(&text) {
        Ok(table) => Ok(Some(table)),
        Err(_) => bail!("{shown} isn't valid TOML"),
    }
}

/// Sets `key` to `value` in the data folder `data`'s settings.toml,
/// keeping what else the file holds (written by hand, or by another
/// tuitok meanwhile); a file that can't be read is left as it is. Written
/// to a new file readable only by you, then put in place whole.
pub fn save(data: &Path, key: &str, value: toml::Value) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let target = path(data);
    let mut table = match read(&target) {
        Ok(table) => table.unwrap_or_default(),
        Err(e) => bail!("{e:#}, so it wasn't changed"),
    };
    table.insert(key.to_owned(), value);
    let text = toml::to_string(&table)?;
    // This process's own: two tuitoks saving at once don't touch each
    // other's.
    let part = data.join(format!("{FILE}.{}.part", std::process::id()));
    let _ = std::fs::remove_file(&part);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&part)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&part, &target));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&part);
        return Err(e).with_context(|| format!("cannot write {}", shown(&target)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_is_kept_with_whatever_else_the_file_holds() {
        use std::os::unix::fs::PermissionsExt;
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/test-settings-{}", std::process::id()));
        crate::config::private_dir(&data).unwrap();
        assert_eq!(load(&data), Settings::default());
        std::fs::write(path(&data), "theme = \"nope\"\nlater = 1\n").unwrap();
        assert_eq!(load(&data).theme, theme::DEFAULT, "not a built-in theme");
        save(&data, "theme", "nord".into()).unwrap();
        assert_eq!(load(&data).theme, "nord");
        let text = std::fs::read_to_string(path(&data)).unwrap();
        assert!(text.contains("later = 1"), "{text}");
        let mode = std::fs::metadata(path(&data)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_dir(&data).unwrap().count(),
            1,
            "nothing half-written"
        );
        // A file that doesn't read is left as it is.
        std::fs::write(path(&data), "theme = [").unwrap();
        assert_eq!(load(&data), Settings::default());
        let e = save(&data, "theme", "nord".into()).unwrap_err();
        assert!(format!("{e:#}").ends_with("isn't valid TOML, so it wasn't changed"));
        assert_eq!(std::fs::read_to_string(path(&data)).unwrap(), "theme = [");
        // A link in its place isn't followed, or written through.
        std::fs::remove_file(path(&data)).unwrap();
        let elsewhere = data.join("elsewhere.toml");
        std::fs::write(&elsewhere, "theme = \"nord\"\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, path(&data)).unwrap();
        assert_eq!(load(&data), Settings::default());
        assert!(save(&data, "theme", "latte".into()).is_err());
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            "theme = \"nord\"\n"
        );
        std::fs::remove_dir_all(&data).unwrap();
    }
}
