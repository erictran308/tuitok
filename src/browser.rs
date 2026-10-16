//! The headless browser a batch of the feed is fetched with: Lightpanda,
//! else Chrome. Started for one batch and closed after (the report measured
//! the browser as tuitok's expensive part), with a cleaned environment, in a
//! process group of its own that's killed as a whole, also when a batch is
//! dropped half way (`Drop`).

use std::ffi::OsString;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
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

/// Lightpanda's DevTools on a free 127.0.0.1 port, which any local program
/// could reach while it runs: it runs only for a batch, telemetry off.
async fn lightpanda(program: &Path) -> Result<Browser> {
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))?
        .local_addr()?
        .port();
    let mut command = Command::new(program);
    command
        .args(["serve", "--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .arg("--disable-metrics")
        .env_clear()
        .envs(tools::child_env())
        .env("LIGHTPANDA_DISABLE_TELEMETRY", "true");
    let mut child = {
        let _starting = tools::starting();
        spawn(&mut command, "Lightpanda")?
    };
    let group = child.id().map(|pid| pid as i32);
    let url = format!("ws://127.0.0.1:{port}/");
    let deadline = Instant::now() + START_WAIT;
    let (cdp, events) = loop {
        match Cdp::websocket(&url).await {
            Ok(connected) => break connected,
            Err(_) if Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                let _ = child.kill().await;
                return Err(e.context("Lightpanda didn't start listening"));
            }
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
        let child = spawn(&mut command, "Chrome")?;
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
fn spawn(command: &mut Command, name: &str) -> Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
}
