//! The headless browser a batch of the feed is fetched with: Lightpanda,
//! else Chrome. Started for one batch and closed after (the report measured
//! the browser as tuitok's expensive part), with a cleaned environment, in a
//! process group of its own that's killed as a whole, also when a batch is
//! dropped half way (`Drop`).

use std::ffi::OsString;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::cdp::{Cdp, Event};
use crate::tools;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Lightpanda,
    Chrome,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::Lightpanda => "Lightpanda",
            Engine::Chrome => "Chrome",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "lightpanda" => Some(Engine::Lightpanda),
            "chrome" => Some(Engine::Chrome),
            _ => None,
        }
    }
}

/// How long a browser may take to start listening.
const START_WAIT: Duration = Duration::from_secs(10);

/// Chrome's flags: headless, its DevTools on the pipe, nothing of Chrome's
/// own over the network (updates, sync, translation, metrics), no Keychain
/// prompt for the profile, no pictures or sound, and fewer processes (the
/// report's "Lighter browsers": site isolation off, one renderer, the GPU
/// in the browser's process).
const CHROME_FLAGS: [&str; 20] = [
    "--headless",
    "--remote-debugging-pipe",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-sync",
    "--disable-default-apps",
    "--disable-extensions",
    "--disable-breakpad",
    "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication,site-per-process,IsolateOrigins",
    "--metrics-recording-only",
    "--use-mock-keychain",
    "--password-store=basic",
    "--mute-audio",
    "--hide-crash-restore-bubble",
    "--blink-settings=imagesEnabled=false",
    "--renderer-process-limit=1",
    "--in-process-gpu",
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
];

pub struct Browser {
    pub engine: Engine,
    pub cdp: Cdp,
    pub events: mpsc::UnboundedReceiver<Event>,
    child: Child,
    /// Its process group (the child's pid), until it's been killed.
    group: Option<i32>,
}

impl Browser {
    /// Starts `engine` from `program`; Chrome keeps its profile in `profile`.
    pub async fn start(engine: Engine, program: &Path, profile: &Path) -> Result<Self> {
        match engine {
            Engine::Lightpanda => lightpanda(program).await,
            Engine::Chrome => chrome(program, profile).await,
        }
    }

    /// Asks Chrome to quit, then ends the browser's whole process group.
    pub async fn close(mut self) {
        if self.engine == Engine::Chrome {
            let quit = self.cdp.call("Browser.close", json!({}), None);
            let _ = tokio::time::timeout(Duration::from_secs(2), quit).await;
        }
        // While the child isn't waited for, its pid (the group's id) can't
        // be taken by another process.
        self.kill_group();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    fn kill_group(&mut self) {
        if let Some(group) = self.group.take() {
            // SAFETY: a plain signal to a process group of tuitok's own.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// Lightpanda's DevTools on a 127.0.0.1 port, which local programs can
/// reach, so:
/// - Lightpanda picks the port itself and names it in its log
///   ([`listening_port`]): no other program can take it between its being
///   chosen and Lightpanda's listening, and be handed tuitok's connection
///   and the guest's cookies.
/// - It takes one client (`--cdp-max-connections 1`): once tuitok is
///   connected, nothing else can drive it, read its cookies or what it
///   fetched, a script in TikTok's page included. A program that got in
///   first leaves tuitok refused before any cookie is sent, and Chrome
///   fetches instead.
/// - Its pages can't reach private or local addresses
///   (`--block-private-networks`): TikTok's scripts can't probe your network
///   or this computer's services from it.
///
/// It runs only for a batch, telemetry off.
async fn lightpanda(program: &Path) -> Result<Browser> {
    let mut command = Command::new(program);
    command
        .args(["serve", "--host", "127.0.0.1", "--port", "0"])
        .args([
            "--cdp-max-connections",
            "1",
            "--block-private-networks",
            "--disable-metrics",
        ])
        .env_clear()
        .envs(tools::child_env())
        .env("LIGHTPANDA_DISABLE_TELEMETRY", "true");
    let mut child = {
        let _starting = tools::starting();
        spawn(&mut command, "Lightpanda", Stdio::piped())?
    };
    let group = child.id().map(|pid| pid as i32);
    let mut log = BufReader::new(child.stderr.take().context("no log from Lightpanda")?);
    let port = tokio::time::timeout(START_WAIT, async {
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut log)
                .take(MAX_LOG_LINE)
                .read_until(b'\n', &mut line)
                .await
            {
                Ok(0) | Err(_) => return None,
                Ok(_) => {
                    if let Some(port) = listening_port(&String::from_utf8_lossy(&line)) {
                        return Some(port);
                    }
                }
            }
        }
    })
    .await;
    let Ok(Some(port)) = port else {
        let _ = child.kill().await;
        anyhow::bail!("Lightpanda didn't start listening");
    };
    // Its log goes on: read to the end, so it never waits on a full pipe.
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut log, &mut tokio::io::sink()).await;
    });
    let (cdp, events) = match Cdp::websocket(&format!("ws://127.0.0.1:{port}/")).await {
        Ok(connected) => connected,
        Err(e) => {
            let _ = child.kill().await;
            return Err(e.context("Lightpanda took no connection from tuitok"));
        }
    };
    Ok(Browser {
        engine: Engine::Lightpanda,
        cdp,
        events,
        child,
        group,
    })
}

/// Chrome with its DevTools on two pipes, as its fds 3 (it reads) and 4 (it
/// writes): no port. Made under `tools::starting`, so no other program
/// started meanwhile gets them.
async fn chrome(program: &Path, profile: &Path) -> Result<Browser> {
    crate::config::private_dir(profile)?;
    let mut user_data = OsString::from("--user-data-dir=");
    user_data.push(profile);
    let mut command = Command::new(program);
    command
        .args(CHROME_FLAGS)
        .arg(user_data)
        .arg("about:blank")
        .env_clear()
        .envs(tools::child_env());
    let (child, to, from) = {
        let _starting = tools::starting();
        let (chrome_reads, to_chrome) = std::io::pipe()?;
        let (from_chrome, chrome_writes) = std::io::pipe()?;
        let (a, b) = (chrome_reads.as_raw_fd(), chrome_writes.as_raw_fd());
        // SAFETY: as_fd3_and_4 makes only async-signal-safe calls.
        unsafe { command.pre_exec(move || as_fd3_and_4(a, b)) };
        let child = spawn(&mut command, "Chrome", Stdio::null())?;
        (child, to_chrome, from_chrome)
    };
    let group = child.id().map(|pid| pid as i32);
    let to = pipe::Sender::from_owned_fd(OwnedFd::from(to))?;
    let from = pipe::Receiver::from_owned_fd(OwnedFd::from(from))?;
    let (cdp, events) = Cdp::pipe(to, from);
    let browser = Browser {
        engine: Engine::Chrome,
        cdp,
        events,
        child,
        group,
    };
    browser
        .cdp
        .call("Browser.getVersion", json!({}), None)
        .await
        .context("Chrome didn't start")?;
    Ok(browser)
}

/// Starts `command` in a process group of its own, nothing on its standard
/// streams. The caller holds `tools::starting`.
/// The longest line of Lightpanda's log read when looking for its port.
const MAX_LOG_LINE: u64 = 4096;

/// The port in Lightpanda's `msg="server running" address=127.0.0.1:PORT`.
fn listening_port(line: &str) -> Option<u16> {
    if !line.contains("msg=\"server running\"") {
        return None;
    }
    let after = line.split_once(" address=127.0.0.1:")?.1;
    let digits = &after[..after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len())];
    digits.parse().ok().filter(|&port: &u16| port != 0)
}

fn spawn(command: &mut Command, name: &str, stderr: Stdio) -> Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start {name}"))
}

/// In the child, before it runs Chrome: `a` as fd 3 and `b` as fd 4, both
/// inherited (`dup2` clears close-on-exec). Only async-signal-safe calls.
fn as_fd3_and_4(a: RawFd, b: RawFd) -> std::io::Result<()> {
    // SAFETY: plain calls on file descriptors the child holds.
    unsafe {
        // Out of the way first, so putting one in place can't close the other.
        let a = libc::fcntl(a, libc::F_DUPFD_CLOEXEC, 10);
        let b = libc::fcntl(b, libc::F_DUPFD_CLOEXEC, 10);
        if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether `engine` can be started: its program was found.
pub fn program(tools: &tools::Tools, engine: Engine) -> Option<&Path> {
    match engine {
        Engine::Lightpanda => tools.lightpanda.as_deref(),
        Engine::Chrome => tools.chrome.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engines_by_name() {
        assert_eq!(Engine::parse("lightpanda"), Some(Engine::Lightpanda));
        assert_eq!(Engine::parse("chrome"), Some(Engine::Chrome));
        assert_eq!(Engine::parse("firefox"), None);
        assert_eq!(Engine::Chrome.name(), "Chrome");
    }

    #[test]
    fn lightpandas_port_from_its_log() {
        let running = "$time=1792142614939 $scope=note $level=note $msg=\"server running\" \
                       address=127.0.0.1:49645 max_connections=16208 max_browser_connections=1\n";
        assert_eq!(listening_port(running), Some(49645));
        assert_eq!(
            listening_port("$msg=\"server running\" address=127.0.0.1:0"),
            None
        );
        assert_eq!(
            listening_port("$msg=\"server running\" address=0.0.0.0:9222"),
            None
        );
        assert_eq!(
            listening_port("$msg=\"page\" url=x address=127.0.0.1:1234"),
            None
        );
        assert_eq!(
            listening_port("$msg=\"server running\" address=127.0.0.1:99999"),
            None
        );
    }
}
