//! The terminal tuitok draws on: taken over at the start (raw mode, the
//! alternate screen, bracketed paste) and handed back at the end, normally
//! or from the panic hook, as tuitube's `screen.rs`. Its tab may be closed
//! with tuitok in it: then every write to it fails, and nothing here may
//! panic (a panic in the panic hook aborts) or report that, which is
//! expected.

use std::io::{IsTerminal, Write, stdout};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::{DefaultTerminal, Terminal};

use crate::text;

/// Whether stderr is the terminal too, not a file: then it goes when the
/// terminal does (`gone`).
static STDERR_IS_IT: AtomicBool = AtomicBool::new(false);

/// Why one of tuitok's tasks panicked, once one has: tuitok then quits as
/// with q, and says it once the terminal is back (`main::ended`).
static TASK_PANIC: OnceLock<String> = OnceLock::new();

/// A plain error, before anything is started, when tuitok isn't run in a
/// terminal (piped, or from a script): there'd be nothing to draw on.
pub fn need() -> Result<()> {
    if !std::io::stdin().is_terminal() || !stdout().is_terminal() {
        bail!("tuitok needs a terminal to run in (tuitok --help says more)");
    }
    Ok(())
}

/// Takes the terminal over. The panic hook goes first, so a panic from here
/// on hands it back (ratatui's own, `ratatui::init`, isn't used: it prints
/// with `eprintln!`). What a step that failed leaves is put back. Pastes
/// come whole (bracketed): a link pasted isn't taken for keys.
pub fn take() -> Result<DefaultTerminal> {
    need()?;
    STDERR_IS_IT.store(same_terminal(), Ordering::Relaxed);
    set_panic_hook();
    let taken = enable_raw_mode()
        .and_then(|()| execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste))
        .and_then(|()| Terminal::new(CrosstermBackend::new(stdout())));
    if taken.is_err() {
        restore();
    }
    Ok(taken?)
}

/// Hands the terminal back at the end, and tmux's pane setting. `terminal`
/// is forgotten, not dropped: its drop shows the cursor, done here already,
/// and prints a failure with `eprintln!`.
pub fn hand_back(terminal: DefaultTerminal) {
    std::mem::forget(terminal);
    restore();
    crate::tmux::restore();
}

/// Why a task panicked, if one did.
pub fn task_panic() -> Option<&'static str> {
    TASK_PANIC.get().map(String::as_str)
}

/// A picture decoder's caught panic passes without tearing the screen down.
/// Another task's would leave tuitok drawing on a terminal put back to
/// normal (tokio catches it and the event loop goes on), so the screen
/// stays and tuitok quits as with q (SIGTERM, which `App::run` takes so):
/// the player quits, Pepper's box goes, and why is said after. The event
/// loop's own (on the main thread) hands the terminal back at once and says
/// why without control characters (it can quote a caption), then unwinds,
/// so the browser is still killed as its `Drop` does.
fn set_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if crate::covers::panic_is_contained() {
            return;
        }
        if std::thread::current().name() != Some("main") {
            let _ = TASK_PANIC.set(text::one_line(&info.to_string(), 400));
            // SAFETY: a plain signal to tuitok itself.
            unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
            return;
        }
        restore();
        crate::tmux::restore();
        // Not eprintln!, which panics when stderr is gone: a panic in this
        // hook aborts.
        let _ = writeln!(
            std::io::stderr(),
            "tuitok crashed: {}",
            text::one_line(&info.to_string(), 400)
        );
    }));
}

/// Puts the terminal back as it was, each step on its own, so one that
/// fails doesn't stop the other, and says on stderr what failed. A
/// terminal that's gone has nothing to put back.
fn restore() {
    if gone() {
        return;
    }
    let raw = disable_raw_mode();
    let screen = execute!(
        stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
    for err in [raw.err(), screen.err()].into_iter().flatten() {
        let _ = writeln!(
            std::io::stderr(),
            "tuitok: the terminal wasn't put back: {err}"
        );
    }
}

/// Whether the terminal is gone: its tab closed with tuitok in it (SIGHUP,
/// then a quit as with `q`). Every write to it fails with EIO then, even of
/// nothing, which a live terminal takes; `isatty` and `tcgetattr` still
/// work while tuitok leads its session, and a SIGHUP may come from `kill`.
/// Once gone, stdout points at /dev/null, and stderr too when it was the
/// terminal: what's still written on the way out, by tuitok or the code it
/// uses, then can't fail, where a failed `eprintln!` panics.
pub fn gone() -> bool {
    use std::os::fd::AsRawFd;
    static GONE: AtomicBool = AtomicBool::new(false);
    if GONE.load(Ordering::Relaxed) {
        return true;
    }
    // SAFETY: a write of no bytes, from a pointer that's valid for none.
    let wrote = unsafe { libc::write(libc::STDOUT_FILENO, [0u8; 0].as_ptr().cast(), 0) };
    if wrote >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EIO) {
        return false;
    }
    GONE.store(true, Ordering::Relaxed);
    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        // SAFETY: dup2 points the fd at /dev/null in place; Rust's stdout and
        // stderr only ever use the fd number.
        unsafe {
            libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO);
            if STDERR_IS_IT.load(Ordering::Relaxed) {
                libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO);
            }
        }
    }
    true
}

/// Whether stderr is the terminal stdout is, rather than a file or another
/// terminal. Asked at the start: once the tab is gone, neither says.
fn same_terminal() -> bool {
    let device = |fd| {
        // SAFETY: fstat only fills in the stat given.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstat(fd, &mut stat) } == 0).then_some(stat.st_rdev)
    };
    std::io::stderr().is_terminal() && device(libc::STDERR_FILENO) == device(libc::STDOUT_FILENO)
}
