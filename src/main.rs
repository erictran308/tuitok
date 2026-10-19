//! tuitok: TikTok's For You feed in the terminal, one upright video at a
//! time, playing inside Pepper (`../pepper`). CLAUDE.md has the research
//! behind it, in short.

mod app;
mod browser;
mod cache;
mod cdp;
mod comments;
mod config;
mod covers;
mod demo;
mod explore;
mod feed;
mod guest;
mod links;
mod pepper;
mod player;
mod settings;
mod text;
mod theme;
mod tmux;
mod tools;
mod ui;
mod video;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;

use browser::Engine;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};

const HELP: &str = "\
tuitok: TikTok's For You feed in the terminal, playing inside Pepper

Usage: tuitok [LINK | --check | --feed [lightpanda | chrome] | --demo | --forget | --help | --version]

  LINK        a TikTok video's link (or its id) to play first, then For You
  --check     say what tuitok found: Pepper, its player, a browser for the feed
  --feed      fetch one batch of the feed and list it: with Lightpanda, else
              Chrome, or with the one named (as a new guest)
  --demo      the screen with made-up videos: nothing is fetched or played
  --forget    forget the last run's guest and the videos you watched, so the
              next run starts over as a new guest
  --help      print this
  --version   print tuitok's version

Keys: j or ↓ the next video, k or ↑ the one before, / For You or one of
Explore's categories, : open <link> plays a TikTok link (or paste one),
space pause, m mute, - and + the volume down and up, h, ← or , and l, → or
. back and on 5 s, f fill the tab, ? every key and the settings (the
theme), r fetch more, q quit.

tuitok isn't made or endorsed by TikTok or ByteDance.";

/// The box `--feed` picks renditions for, in pixels: about an upright box
/// in a laptop's Pepper window.
const LIST_BOX: (u32, u32) = (608, 1080);

/// Why one of tuitok's tasks panicked, once one has: tuitok then quits as
/// with q, and says it once the terminal is back ([`start_screen`]).
static TASK_PANIC: OnceLock<String> = OnceLock::new();

fn main() -> ExitCode {
    // Before any thread starts: code tuitok uses starts programs by name
    // (crossterm's `tput`), and none may come from a relative PATH folder.
    let path = tools::absolute_path(&std::env::var_os("PATH").unwrap_or_default());
    // SAFETY: no other thread exists yet to read the environment meanwhile.
    unsafe { std::env::set_var("PATH", path) };

    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => run(None),
        ["--check"] => {
            check();
            ExitCode::SUCCESS
        }
        ["--feed"] => list_feed(None),
        ["--demo"] => run_demo(),
        ["--forget"] => forget_guest(),
        ["--feed", name] if Engine::parse(name).is_some() => list_feed(Engine::parse(name)),
        ["--help" | "-h"] => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        ["--version" | "-V"] => {
            println!("tuitok {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [link] => match links::parse(link) {
            Some(link) => run(Some(link)),
            None => unknown_arguments(),
        },
        _ => unknown_arguments(),
    }
}

/// Arguments tuitok doesn't take. What was typed isn't repeated: it could
/// hold escape sequences.
fn unknown_arguments() -> ExitCode {
    eprintln!("tuitok: unknown arguments (tuitok --help)");
    ExitCode::from(2)
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}

fn fail(e: &anyhow::Error) -> ExitCode {
    eprintln!("tuitok: {}", text::one_line(&format!("{e:#}"), 400));
    ExitCode::FAILURE
}

/// How a run ended: its error, else a task's panic, else well.
fn ended(result: std::io::Result<()>) -> ExitCode {
    match (result, TASK_PANIC.get()) {
        (Err(e), _) => fail(&e.into()),
        (Ok(()), Some(why)) => {
            eprintln!("tuitok: a bug ended it: {why}");
            ExitCode::FAILURE
        }
        (Ok(()), None) => ExitCode::SUCCESS,
    }
}

/// A run, playing `first`'s video first if it's given (`tuitok LINK`).
fn run(first: Option<links::Link>) -> ExitCode {
    let session = match config::Session::start() {
        Ok(session) => session,
        Err(e) => return fail(&e),
    };
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(e) => return fail(&e.into()),
    };
    let tools = tools::Tools::find();
    let port = pepper::port();
    // pepper-play in Pepper, else mpv in its window.
    let player_with = match (&port, &tools.pepper_play, &tools.mpv) {
        (Some(port), Some(play), _) => Some((play.clone(), player::Kind::Pepper(port.clone()))),
        (_, _, Some(mpv)) => Some((mpv.clone(), player::Kind::Window)),
        _ => None,
    };
    let engines = feed::engines(&tools, None);
    let mut app = app::App::new(port.is_some(), engines, &session, player_with);
    // The last run's guest: the same TikTok guest goes on, and what was
    // watched isn't shown again.
    app.carry_in(guest::load(&session.data));
    app.use_settings(settings::load(&session.data), Some(session.data.clone()));
    if let Some(link) = first {
        app.open_first(link);
    }
    let mut terminal = ratatui::init();
    start_screen(&mut app);
    let result = runtime.block_on(app.run(&mut terminal));
    bracketed_paste(false);
    ratatui::restore();
    tmux::restore();
    // A batch still being fetched and the downloads end here, the browser
    // killed (`Browser`'s `Drop`), before the session's folder goes.
    drop(runtime);
    if let Err(e) = guest::save(&session.data, &app.carry_out()) {
        eprintln!(
            "tuitok: couldn't keep the guest for next time: {}",
            text::one_line(&format!("{e:#}"), 300)
        );
    }
    if let Err(e) = app.keep_volume() {
        eprintln!(
            "tuitok: couldn't keep the volume for next time: {}",
            text::one_line(&format!("{e:#}"), 300)
        );
    }
    session.end();
    ended(result)
}

/// `--demo`: the real screen with made-up videos; nothing is fetched,
/// downloaded or played, and no folder is made.
fn run_demo() -> ExitCode {
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(e) => return fail(&e.into()),
    };
    let mut app = app::App::demo(pepper::port().is_some());
    // Your theme, but nothing is saved: the demo makes no folder.
    if let Ok(data) = config::data_dir_path() {
        app.use_settings(settings::load(&data), None);
    }
    let mut terminal = ratatui::init();
    start_screen(&mut app);
    let result = runtime.block_on(app.run(&mut terminal));
    bracketed_paste(false);
    ratatui::restore();
    tmux::restore();
    ended(result)
}

/// Once the terminal is set up and before keys are read: how it draws
/// pictures (`covers::picker` asks it), pastes whole (a link pasted isn't
/// taken for keys), and a panic hook. A picture
/// decoder's caught panic passes without tearing the screen down. Another
/// task's would leave tuitok drawing on a terminal put back to normal
/// (tokio catches it and the event loop goes on), so the screen stays and
/// tuitok quits as with q (SIGTERM, which `App::run` takes so): the player
/// quits, Pepper's box goes, and why is said after ([`ended`]). Only the
/// event loop's own (on the main thread) restores the terminal at once
/// (ratatui's hook, set by `ratatui::init`).
fn start_screen(app: &mut app::App) {
    tmux::save();
    app.set_picker(covers::picker());
    app.behind_multiplexer = pepper::behind_multiplexer();
    bracketed_paste(true);
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if covers::panic_is_contained() {
            return;
        }
        if std::thread::current().name() != Some("main") {
            let _ = TASK_PANIC.set(text::one_line(&info.to_string(), 400));
            // SAFETY: a plain signal to tuitok itself.
            unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
            return;
        }
        bracketed_paste(false);
        tmux::restore();
        restore(info);
    }));
}

/// Asks the terminal to send pastes whole (bracketed), or not any more.
fn bracketed_paste(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        crossterm::execute!(out, EnableBracketedPaste)
    } else {
        crossterm::execute!(out, DisableBracketedPaste)
    };
}

/// `--check`: what tuitok found, and where.
fn check() {
    let tools = tools::Tools::find();
    let found = |path: &Option<PathBuf>| {
        path.as_deref()
            .map_or_else(|| "not found".to_owned(), tools::shown)
    };
    match (pepper::port(), pepper::behind_multiplexer()) {
        (Some(_), _) => println!("Pepper:      yes, videos play in this tab"),
        (None, Some(name)) => println!(
            "Pepper:      yes, but inside {name}, through which its box can't be placed: \
             videos play in mpv's window"
        ),
        (None, None) => println!("Pepper:      no: videos play in mpv's window"),
    }
    println!("pepper-play: {}", found(&tools.pepper_play));
    println!("Lightpanda:  {}", found(&tools.lightpanda));
    println!("Chrome:      {}", found(&tools.chrome));
    println!("mpv:         {}", found(&tools.mpv));
    let last = config::data_dir_path()
        .map(|data| guest::load(&data))
        .unwrap_or_default();
    if last == guest::Guest::default() {
        println!("Last guest:  none: the next run starts as a new guest");
    } else {
        println!(
            "Last guest:  {} cookies, {} videos watched (tuitok --forget starts over)",
            last.cookies.len(),
            last.watched.len()
        );
    }
}

/// `--forget`: deletes the last run's guest.
fn forget_guest() -> ExitCode {
    let forgot = config::data_dir().and_then(|data| guest::forget(&data));
    match forgot {
        Ok(true) => println!("Forgot the last guest and the videos you watched."),
        Ok(false) => println!("Nothing to forget: no guest was kept."),
        Err(e) => return fail(&e),
    }
    ExitCode::SUCCESS
}

/// `--feed`: one batch, listed. Never the cookie itself.
fn list_feed(only: Option<Engine>) -> ExitCode {
    let session = match config::Session::start() {
        Ok(session) => session,
        Err(e) => return fail(&e),
    };
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(e) => return fail(&e.into()),
    };
    let engines = feed::engines(&tools::Tools::find(), only);
    let result = runtime.block_on(feed::fetch_any(
        engines,
        session.profile.clone(),
        feed::Ask {
            want: feed::BATCH,
            ..feed::Ask::default()
        },
        &|_| {},
    ));
    drop(runtime);
    session.end();
    let batch = match result {
        Ok(batch) => batch,
        Err(e) => return fail(&e),
    };
    println!(
        "{}: {} videos in {:.1} s (the first after {}), tt_chain_token {}",
        batch.engine.name(),
        batch.videos.len(),
        batch.took.as_secs_f64(),
        batch
            .first
            .map_or_else(|| "-".to_owned(), |t| format!("{:.1} s", t.as_secs_f64())),
        if batch.token.is_some() { "yes" } else { "no" }
    );
    if let Some(note) = &batch.note {
        println!("(before it: {note})");
    }
    for video in &batch.videos {
        let play = video.best(LIST_BOX).map_or_else(
            || "-".to_owned(),
            |r| {
                let size = r.bytes.map_or_else(|| "?".to_owned(), ui::size);
                format!("{} {}×{} {size}", r.codec.name(), r.width, r.height)
            },
        );
        println!(
            "{}  @{:<24} {:>5}  {:<24} {}",
            video.id.as_str(),
            text::first_chars(&video.author, 24),
            ui::clock(video.seconds),
            play,
            text::one_line(&video.caption, 48)
        );
    }
    ExitCode::SUCCESS
}
