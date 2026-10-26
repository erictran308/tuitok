//! The player: one pepper-play (in Pepper) or mpv (elsewhere, in a window
//! of its own) for the whole run, idle until told what to play. It's driven
//! over mpv's JSON IPC on one end of a socketpair passed as its fd 3
//! (`--input-ipc-client=fd://3`), as tuitube drives it: no socket file,
//! nothing about a video on its command line, and it quits when tuitok's
//! end closes. j and k load the next video into the same player (`loadfile
//! replace`), as pepper-agent does, and each video loops, as on TikTok.
//!
//! In Pepper the picture shows in a box of cells tuitok places
//! (`pepper::place`) with a token pepper-play gets over the socket only.

use std::ffi::OsString;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::{cache, pepper, text, tools};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// pepper-play, drawing into Pepper's box: Pepper's port.
    Pepper(OsString),
    /// mpv in a window of its own.
    Window,
    /// mpv with no picture or sound, for tests.
    #[cfg(test)]
    Headless,
}

/// What to play: a downloaded file, or an address streamed with its
/// batch's cookie.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    File(PathBuf),
    Stream {
        url: String,
        token: Option<Arc<str>>,
    },
}

/// How loud a player starts: the volume in percent, and whether it's muted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sound {
    pub volume: u32,
    pub muted: bool,
}

/// What the player says. What it says of a file names the playlist entry
/// it played it as, so what an older file said (one ending or looping just
/// as the next was loaded) isn't taken for the next one's.
#[derive(Debug, PartialEq)]
pub enum PlayerEvent {
    /// It took the load numbered `load` ([`Player::load`]) as playlist
    /// entry `entry`.
    Loaded {
        load: u64,
        entry: i64,
    },
    /// The first frame of entry `entry` is ready (and again after each loop
    /// or seek).
    Restarted(i64),
    /// Where entry `entry` is, in seconds, said as the whole second changes
    /// (the panel shows no finer), and how long it is.
    Position(i64, f64),
    Duration(i64, f64),
    Paused(bool),
    /// The volume, in percent, and the sound off or on: as tuitok set
    /// them, or as they were changed in mpv's window.
    Volume(f64),
    Muted(bool),
    /// Entry `entry` can't play.
    Failed(i64, String),
    /// The player is gone: its window closed, or it ended.
    Exited,
}

/// Every player's options: nothing of the user's mpv setup (config,
/// scripts), no terminal, no yt-dlp, certificates checked (mpv's default
/// takes any), nothing remembered between runs, decoding on the graphics
/// chip, idle until told what to play, each video looping and kept on its
/// last frame.
const OPTIONS: [&str; 12] = [
    "--no-config",
    "--load-scripts=no",
    "--terminal=no",
    "--ytdl=no",
    "--tls-verify=yes",
    "--save-position-on-quit=no",
    "--resume-playback=no",
    "--hwdec=auto-safe",
    "--idle=yes",
    "--keep-open=yes",
    "--loop-file=inf",
    "--input-ipc-client=fd://3",
];

/// How long the player gets to quit before it's killed.
const QUIT_GRACE: Duration = Duration::from_secs(1);

pub struct Player {
    out: mpsc::UnboundedSender<String>,
    /// Pepper's token for this player's picture (pepper-play only).
    pub token: Option<String>,
    /// Loads so far: each is numbered, and its answer names the number.
    loads: AtomicU64,
    child: Child,
}

impl Player {
    /// Starts `program` as `kind`, as loud as `sound`; what it says goes to
    /// `events`.
    pub fn start(
        program: &Path,
        kind: &Kind,
        sound: Sound,
        events: impl Fn(PlayerEvent) + Send + 'static,
    ) -> Result<Self> {
        let token = match kind {
            Kind::Pepper(_) => {
                Some(pepper::new_token().context("no randomness for Pepper's token")?)
            }
            _ => None,
        };
        let mut command = Command::new(program);
        command.args(OPTIONS).env_clear().envs(tools::child_env());
        // On macOS, mpv's CoreAudio output doesn't open for 48 kHz sound in
        // planar float and falls back to AVFoundation, which holds 2 s of
        // sound (tuitube's finding); interleaved float opens it.
        if cfg!(target_os = "macos") {
            command.arg("--audio-format=float");
        }
        match kind {
            Kind::Pepper(port) => {
                command.env("PEPPER_PORT", port);
            }
            Kind::Window => {
                command.args([
                    "--force-window=immediate",
                    "--title=tuitok",
                    "--autofit-larger=40%x85%",
                ]);
            }
            #[cfg(test)]
            Kind::Headless => {
                command.args(["--vo=null", "--ao=null", "--force-window=no"]);
            }
        }
        if let Some(home) = dirs::home_dir() {
            command.current_dir(home);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (child, ours) = {
            let _starting = tools::starting();
            let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
            let fd = theirs.as_raw_fd();
            // SAFETY: as_fd3 makes only async-signal-safe calls.
            unsafe { command.pre_exec(move || as_fd3(fd)) };
            let child = command
                .spawn()
                .with_context(|| format!("cannot start {}", tools::shown(program)))?;
            (child, ours)
        };
        ours.set_nonblocking(true)?;
        let (read, mut write) = tokio::net::UnixStream::from_std(ours)?.into_split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = outgoing.recv().await {
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            let mut heard = Heard::default();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(event) = heard.parse(&line) {
                    events(event);
                }
            }
            events(PlayerEvent::Exited);
        });
        let player = Self {
            out,
            token,
            loads: AtomicU64::new(0),
            child,
        };
        // Before the volume is observed, so the first value it says is this.
        player.set_volume(sound.volume);
        player.set_muted(sound.muted);
        let observed = [
            (1, "time-pos"),
            (2, "duration"),
            (3, "pause"),
            (4, "volume"),
            (5, "mute"),
        ];
        for (id, name) in observed {
            player.command(json!(["observe_property", id, name]));
        }
        player.command(json!(["set_property", "user-agent", cache::USER_AGENT]));
        player.command(json!(["set_property", "referrer", cache::REFERER]));
        if let Some(token) = &player.token {
            player.command(json!(["script-message-to", "main", "pepper-token", token]));
        }
        Ok(player)
    }

    /// One command, as JSON (so nothing in it can break out of it).
    fn command(&self, command: Value) {
        self.send(json!({ "command": command }));
    }

    fn send(&self, message: Value) {
        let _ = self.out.send(format!("{message}\n"));
    }

    /// Plays `target` in place of what plays, `paused` or not (mpv keeps
    /// `pause` from one file to the next). A stream's cookie is set just
    /// before, over the socket; a file clears it. Gives the load's number,
    /// which [`PlayerEvent::Loaded`] names with the playlist entry it made.
    pub fn load(&self, target: &Target, paused: bool) -> u64 {
        let (what, headers): (String, Vec<String>) = match target {
            Target::File(path) => (path.to_string_lossy().into_owned(), Vec::new()),
            Target::Stream { url, token } => (
                url.clone(),
                token
                    .iter()
                    .map(|t| format!("Cookie: tt_chain_token={t}"))
                    .collect(),
            ),
        };
        self.command(json!(["set_property", "http-header-fields", headers]));
        self.command(json!(["set_property", "pause", paused]));
        let load = self.loads.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(json!({ "command": ["loadfile", what, "replace"], "request_id": load }));
        load
    }

    /// Stops playing, staying open for the next `load`.
    pub fn stop(&self) {
        self.command(json!(["stop"]));
    }

    pub fn toggle_pause(&self) {
        self.command(json!(["cycle", "pause"]));
    }

    /// Pauses, if it plays: unlike [`Player::toggle_pause`], never plays.
    pub fn pause(&self) {
        self.command(json!(["set_property", "pause", true]));
    }

    /// Sets the volume, in percent.
    pub fn set_volume(&self, percent: u32) {
        self.command(json!(["set_property", "volume", percent.min(100)]));
    }

    pub fn set_muted(&self, muted: bool) {
        self.command(json!(["set_property", "mute", muted]));
    }

    pub fn toggle_fullscreen(&self) {
        self.command(json!(["cycle", "fullscreen"]));
    }

    pub fn seek(&self, seconds: f64) {
        self.command(json!(["seek", seconds, "relative"]));
    }

    /// Asks the player to quit, and kills it after `QUIT_GRACE`.
    pub async fn quit(mut self) {
        self.command(json!(["quit"]));
        if tokio::time::timeout(QUIT_GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// In the child, before it runs: the socket as fd 3, inherited (`dup2`
/// clears close-on-exec, or it's cleared if the socket is fd 3 already).
fn as_fd3(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: plain calls on a file descriptor the child holds.
    let ok = unsafe {
        if fd == 3 {
            libc::fcntl(3, libc::F_SETFD, 0) != -1
        } else {
            libc::dup2(fd, 3) != -1
        }
    };
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// What the player's messages mean, read in order: the playlist entry it
/// plays (its last `start-file`), which what it says of the file is
/// about, and the whole second of the last position said.
#[derive(Default)]
struct Heard {
    entry: Option<i64>,
    second: Option<(i64, u64)>,
}

impl Heard {
    /// What one of mpv's messages means to tuitok, if anything. A position
    /// goes on only when its whole second changes: mpv says one each frame,
    /// and each is a redraw.
    fn parse(&mut self, line: &str) -> Option<PlayerEvent> {
        let message: Value = serde_json::from_str(line).ok()?;
        let number = |value: &Value| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0);
        // Only loads carry a number of their own; other commands' answers are 0.
        if let Some(load) = message["request_id"].as_u64().filter(|&n| n > 0) {
            let entry = message["data"]["playlist_entry_id"].as_i64()?;
            return Some(PlayerEvent::Loaded { load, entry });
        }
        match message["event"].as_str()? {
            "start-file" => {
                self.entry = message["playlist_entry_id"].as_i64();
                self.second = None;
                None
            }
            "playback-restart" => self.entry.map(PlayerEvent::Restarted),
            "end-file" if message["reason"] == "error" => Some(PlayerEvent::Failed(
                message["playlist_entry_id"].as_i64()?,
                text::one_line(
                    message["file_error"]
                        .as_str()
                        .unwrap_or("it can't be played"),
                    120,
                ),
            )),
            "property-change" => match message["name"].as_str()? {
                "time-pos" => {
                    let (entry, seconds) = (self.entry?, number(&message["data"])?);
                    let second = Some((entry, seconds as u64));
                    if second == self.second {
                        return None;
                    }
                    self.second = second;
                    Some(PlayerEvent::Position(entry, seconds))
                }
                "duration" => Some(PlayerEvent::Duration(
                    self.entry?,
                    number(&message["data"])?,
                )),
                "pause" => message["data"].as_bool().map(PlayerEvent::Paused),
                "volume" => number(&message["data"]).map(PlayerEvent::Volume),
                "mute" => message["data"].as_bool().map(PlayerEvent::Muted),
                _ => None,
            },
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpvs_messages() {
        let mut heard = Heard::default();
        let mut parse = |line: &str| heard.parse(line);
        // Nothing of a file is said before one starts.
        assert_eq!(parse(r#"{"event":"playback-restart"}"#), None);
        assert_eq!(
            parse(r#"{"data":{"playlist_entry_id":3},"request_id":2,"error":"success"}"#),
            Some(PlayerEvent::Loaded { load: 2, entry: 3 })
        );
        assert_eq!(parse(r#"{"request_id":0,"error":"success"}"#), None);
        assert_eq!(
            parse(r#"{"event":"start-file","playlist_entry_id":3}"#),
            None
        );
        assert_eq!(
            parse(r#"{"event":"playback-restart"}"#),
            Some(PlayerEvent::Restarted(3))
        );
        assert_eq!(
            parse(r#"{"event":"property-change","id":1,"name":"time-pos","data":3.5}"#),
            Some(PlayerEvent::Position(3, 3.5))
        );
        // The same second again: nothing to redraw.
        assert_eq!(
            parse(r#"{"event":"property-change","id":1,"name":"time-pos","data":3.9}"#),
            None
        );
        assert_eq!(
            parse(r#"{"event":"property-change","id":1,"name":"time-pos","data":4.0}"#),
            Some(PlayerEvent::Position(3, 4.0))
        );
        assert_eq!(
            parse(r#"{"event":"property-change","name":"time-pos","data":null}"#),
            None
        );
        assert_eq!(
            parse(r#"{"event":"property-change","name":"duration","data":9.0}"#),
            Some(PlayerEvent::Duration(3, 9.0))
        );
        assert_eq!(
            parse(r#"{"event":"property-change","name":"pause","data":true}"#),
            Some(PlayerEvent::Paused(true))
        );
        assert_eq!(
            parse(r#"{"event":"property-change","id":4,"name":"volume","data":45.0}"#),
            Some(PlayerEvent::Volume(45.0))
        );
        assert_eq!(
            parse(r#"{"event":"property-change","id":5,"name":"mute","data":true}"#),
            Some(PlayerEvent::Muted(true))
        );
        assert_eq!(
            parse(
                r#"{"event":"end-file","reason":"error","playlist_entry_id":3,"file_error":"loading failed\u001b]0;x"}"#
            ),
            Some(PlayerEvent::Failed(3, "loading failed]0;x".into()))
        );
        assert_eq!(parse(r#"{"event":"end-file","reason":"stop"}"#), None);
        assert_eq!(parse("not json"), None);
        // The next file starts: its position is said from its first second.
        parse(r#"{"event":"start-file","playlist_entry_id":4}"#);
        assert_eq!(
            parse(r#"{"event":"property-change","id":1,"name":"time-pos","data":4.2}"#),
            Some(PlayerEvent::Position(4, 4.2))
        );
    }

    /// Plays a video of the feed in mpv with no picture or sound, as tuitok
    /// would: `cargo test -- --ignored live`.
    #[tokio::test]
    #[ignore]
    async fn live_player() {
        let tools = tools::Tools::find();
        let mpv = tools.mpv.clone().expect("mpv");
        let place = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/live-player");
        crate::config::private_dir(&place).unwrap();
        let (engine, program) = crate::feed::engines(&tools, None).remove(0);
        let ask = crate::feed::Ask {
            want: 10,
            ..Default::default()
        };
        let batch = crate::feed::fetch(engine, &program, &place.join("chrome"), &ask, &|_| {})
            .await
            .unwrap();
        let video = &batch.videos[0];
        let rendition = video.best((608, 1080)).unwrap().clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sound = Sound {
            volume: 100,
            muted: false,
        };
        let player = Player::start(&mpv, &Kind::Headless, sound, move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        // Streamed with its cookie, then from a file.
        player.load(
            &Target::Stream {
                url: rendition.urls[0].clone(),
                token: video.token.clone(),
            },
            false,
        );
        /// Whether a first frame comes within 20 s.
        async fn first_frame(rx: &mut mpsc::UnboundedReceiver<PlayerEvent>) -> bool {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(PlayerEvent::Restarted(_))) => return true,
                    Ok(Some(PlayerEvent::Failed(_, e))) => panic!("{e}"),
                    Ok(Some(_)) => {}
                    _ => return false,
                }
            }
        }
        assert!(first_frame(&mut rx).await, "no frame from the stream");
        let job = cache::Job {
            path: place.join(cache::file_name(&video.id, 1, &rendition)),
            id: video.id.clone(),
            rendition,
            token: video.token.clone(),
        };
        let client = cache::client(crate::video::video_url_allowed);
        let cache::Download::Ready(path) = cache::download(&client, &job, |_, _| {}).await else {
            panic!("no download");
        };
        player.load(&Target::File(path), false);
        assert!(first_frame(&mut rx).await, "no frame from the file");
        player.quit().await;
        let _ = std::fs::remove_dir_all(place);
    }
}
