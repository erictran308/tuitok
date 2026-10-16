//! tuitok: TikTok's For You feed in the terminal, one upright video at a
//! time, playing inside Pepper (`../pepper`). The research behind it is
//! `reports/TikTok terminal client.md`.

mod app;
mod browser;
mod cache;
mod cdp;
mod config;
mod feed;
mod pepper;
mod text;
mod theme;
mod tools;
mod ui;
mod video;

use std::path::PathBuf;
use std::process::ExitCode;

use browser::Engine;

const HELP: &str = "\
tuitok: TikTok's For You feed in the terminal, playing inside Pepper

Usage: tuitok [--check | --feed [lightpanda | chrome] | --help | --version]

  --check     say what tuitok found: Pepper, its player, a browser for the feed
  --feed      fetch one batch of the feed and list it: with Lightpanda, else
              Chrome, or with the one named
  --help      print this
  --version   print tuitok's version

Keys: j or ↓ the next video, k or ↑ the one before, r fetch more, q quit.

tuitok isn't made or endorsed by TikTok or ByteDance.";

/// The box `--feed` picks renditions for, in pixels: about an upright box
/// in a laptop's Pepper window.
const LIST_BOX: (u32, u32) = (608, 1080);

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
        [] => run(),
        ["--check"] => {
            check();
            ExitCode::SUCCESS
        }
        ["--feed"] => list_feed(None),
        ["--feed", name] if Engine::parse(name).is_some() => list_feed(Engine::parse(name)),
        ["--help" | "-h"] => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        ["--version" | "-V"] => {
            println!("tuitok {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        // What was typed isn't repeated: it could hold escape sequences.
        _ => {
            eprintln!("tuitok: unknown arguments (tuitok --help)");
            ExitCode::from(2)
        }
    }
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

fn run() -> ExitCode {
    let session = match config::Session::start() {
        Ok(session) => session,
        Err(e) => return fail(&e),
    };
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(e) => return fail(&e.into()),
    };
    let engines = feed::engines(&tools::Tools::find(), None);
    let mut app = app::App::new(pepper::port().is_some(), engines, &session);
    let mut terminal = ratatui::init();
    let result = runtime.block_on(app.run(&mut terminal));
    ratatui::restore();
    // A batch still being fetched and the downloads end here, the browser
    // killed (`Browser`'s `Drop`), before the session's folder goes.
    drop(runtime);
    session.end();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.into()),
    }
}

/// `--check`: what tuitok found, and where.
fn check() {
    let tools = tools::Tools::find();
    let found = |path: &Option<PathBuf>| {
        path.as_deref()
            .map_or_else(|| "not found".to_owned(), tools::shown)
    };
    match pepper::port() {
        Some(_) => println!("Pepper:      yes, videos play in this tab"),
        None => println!("Pepper:      no (not in Pepper, or inside tmux, screen or zellij)"),
    }
    println!("pepper-play: {}", found(&tools.pepper_play));
    println!("Lightpanda:  {}", found(&tools.lightpanda));
    println!("Chrome:      {}", found(&tools.chrome));
    println!("mpv:         {}", found(&tools.mpv));
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
        feed::WANT,
    ));
    drop(runtime);
    session.end();
    let batch = match result {
        Ok(batch) => batch,
        Err(e) => return fail(&e),
    };
    println!(
        "{}: {} videos in {:.1} s, tt_chain_token {}",
        batch.engine.name(),
        batch.videos.len(),
        batch.took.as_secs_f64(),
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
