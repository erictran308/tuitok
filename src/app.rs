//! What tuitok holds, and its event loop: one `tokio::select!` over the
//! terminal's events and tuitok's own (a batch of the feed, a download's
//! progress and end, what the player says), a redraw after each, and after
//! the frame, in Pepper, where the picture goes.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::browser::Engine;
use crate::cache::{self, Download};
use crate::config::Session;
use crate::feed::{self, Batch};
use crate::pepper::{self, Placed};
use crate::player::{Kind, Player, PlayerEvent, Target};
use crate::text;
use crate::theme::{self, Colors};
use crate::ui;
use crate::video::{Video, VideoId};

/// A cell's size in pixels when the terminal doesn't say: about what most
/// fonts make, so the box is still about upright.
const GUESSED_CELL: (u16, u16) = (9, 18);

/// The terminal's size in cells when it doesn't say.
const GUESSED_SIZE: (u16, u16) = (120, 40);

/// More of the feed is fetched once this few videos are left after the one
/// shown.
const REFILL_AT: usize = 5;

/// After a batch fails, how long before one is tried again on its own (`r`
/// tries at once).
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// How far `h` and `l` seek, in seconds.
const SEEK: f64 = 5.0;

const NO_PLAYER: &str = "No player: brew install mpv";

/// What tuitok's own tasks tell the event loop.
pub enum AppEvent {
    Batch(Result<Batch, String>),
    /// How far a download has got.
    Progress {
        id: VideoId,
        got: u64,
        of: Option<u64>,
    },
    /// A download ended: ready, to stream, or failed.
    Downloaded {
        id: VideoId,
        outcome: Download,
    },
    /// What player number `player` said: an older player's words are dropped.
    Player {
        player: u64,
        event: PlayerEvent,
    },
}

/// Where the feed is at, for the status line.
#[derive(Clone, Debug, PartialEq)]
pub enum FeedState {
    /// No browser to fetch it with.
    NoBrowser,
    Fetching(Engine),
    /// The last batch: the videos it added, the browser that got them, how
    /// long it took, and why one before that browser failed.
    Fetched {
        new: usize,
        engine: Engine,
        took: Duration,
        note: Option<String>,
    },
    Failed(String),
}

/// A step towards having the next videos downloaded ([`App::plan`]).
enum Step {
    Start(cache::Job),
    Mark(VideoId, Download),
}

pub struct App {
    pub colors: Colors,
    /// tuitok runs in Pepper (`pepper::port`).
    pub pepper: bool,
    /// A cell's size in pixels, and the terminal's size in cells: the box's
    /// size in pixels comes from both.
    pub cell: (u16, u16),
    pub size: (u16, u16),
    /// The feed so far, each video once.
    pub videos: Vec<Video>,
    /// The video shown.
    pub at: usize,
    pub feed: FeedState,
    /// Downloads by video: started, ready, to stream or failed.
    pub downloads: HashMap<VideoId, Download>,
    /// The video the player shows (its first frame came), where it is in
    /// it, how long it is, and whether it's paused.
    pub playing: Option<VideoId>,
    pub position: f64,
    pub duration: Option<f64>,
    pub paused: bool,
    /// Why the player couldn't start, or the video shown can't play.
    pub play_error: Option<String>,
    engines: Vec<(Engine, PathBuf)>,
    profile: PathBuf,
    cache: PathBuf,
    seen: HashSet<VideoId>,
    fetching: bool,
    failed_at: Option<Instant>,
    /// Downloads running.
    running: usize,
    /// The program to play with and how (`main` decides: pepper-play in
    /// Pepper, else mpv's window), the player once started, how many have
    /// been started (each one's events carry its number), and what it was
    /// told to play.
    player_with: Option<(PathBuf, Kind)>,
    player: Option<Player>,
    players: u64,
    loaded: Option<VideoId>,
    /// Pepper's token for the player's picture (pepper-play only), and what
    /// Pepper was last told.
    token: Option<String>,
    placed: Option<Placed>,
    /// The client for the videos, and where tasks send `AppEvent`s, once
    /// `run` has started.
    client: Option<reqwest::Client>,
    tx: Option<mpsc::UnboundedSender<AppEvent>>,
    quit: bool,
}

impl App {
    /// `engines` are the browsers to fetch the feed with, in order
    /// (`feed::engines`); `session` has the folders for Chrome's profile and
    /// the downloads; `player_with` is what plays, if anything can.
    pub fn new(
        pepper: bool,
        engines: Vec<(Engine, PathBuf)>,
        session: &Session,
        player_with: Option<(PathBuf, Kind)>,
    ) -> Self {
        let feed = match engines.first() {
            Some(&(engine, _)) => FeedState::Fetching(engine),
            None => FeedState::NoBrowser,
        };
        Self {
            colors: Colors::named(theme::DEFAULT),
            pepper,
            cell: cell_size(),
            size: crossterm::terminal::size().unwrap_or(GUESSED_SIZE),
            videos: Vec::new(),
            at: 0,
            feed,
            downloads: HashMap::new(),
            playing: None,
            position: 0.0,
            duration: None,
            paused: false,
            play_error: None,
            engines,
            profile: session.profile.clone(),
            cache: session.cache.clone(),
            seen: HashSet::new(),
            fetching: false,
            failed_at: None,
            running: 0,
            player_with,
            player: None,
            players: 0,
            loaded: None,
            token: None,
            placed: None,
            client: None,
            tx: None,
            quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let (tx, mut app_events) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        self.client = Some(cache::client());
        self.fetch_more();
        let mut terminal_events = EventStream::new();
        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, self))?;
            let placement = self.placement();
            write_to_terminal(&placement)?;
            tokio::select! {
                event = terminal_events.next() => match event {
                    Some(Ok(event)) => self.on_event(event),
                    Some(Err(e)) => return Err(e),
                    None => break,
                },
                Some(event) = app_events.recv() => self.on_app_event(event),
            }
        }
        if let Some(placed) = self.placed.take() {
            write_to_terminal(&pepper::remove(&placed.token))?;
        }
        if let Some(player) = self.player.take() {
            player.quit().await;
        }
        Ok(())
    }

    /// The video shown, if there is one.
    pub fn current(&self) -> Option<&Video> {
        self.videos.get(self.at)
    }

    /// The video box, laid out as `ui::draw` lays it out.
    fn video_area(&self) -> Rect {
        ui::areas(Rect::new(0, 0, self.size.0, self.size.1), self.cell).video
    }

    /// The video box's size in pixels.
    pub fn box_pixels(&self) -> (u32, u32) {
        let video = self.video_area();
        (
            u32::from(video.width) * u32::from(self.cell.0),
            u32::from(video.height) * u32::from(self.cell.1),
        )
    }

    /// Whether the player shows the video shown.
    pub fn shows_picture(&self) -> bool {
        self.playing.is_some() && self.playing.as_ref() == self.current().map(|v| &v.id)
    }

    /// Whether that picture is in Pepper's box (not in mpv's window).
    pub fn picture_in_box(&self) -> bool {
        self.shows_picture() && self.token.is_some()
    }

    /// Whether the player was told to play the video shown and has no
    /// frame of it yet.
    pub fn loading(&self) -> bool {
        self.loaded.is_some()
            && self.loaded.as_ref() == self.current().map(|v| &v.id)
            && !self.shows_picture()
    }

    /// Starts fetching a batch, unless one is being fetched.
    fn fetch_more(&mut self) {
        if self.fetching {
            return;
        }
        let (Some(tx), Some(&(engine, _))) = (&self.tx, self.engines.first()) else {
            return;
        };
        self.fetching = true;
        self.feed = FeedState::Fetching(engine);
        let (tx, engines, profile) = (tx.clone(), self.engines.clone(), self.profile.clone());
        tokio::spawn(async move {
            let batch = feed::fetch_any(engines, profile, feed::WANT)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Batch(batch));
        });
    }

    fn on_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Batch(batch) => {
                self.on_batch(batch);
                self.prefetch();
                self.play_current();
            }
            AppEvent::Progress { id, got, of } => {
                if let Some(Download::Getting {
                    got: was,
                    of: total,
                }) = self.downloads.get_mut(&id)
                {
                    *was = got;
                    *total = of.or(*total);
                }
            }
            AppEvent::Downloaded { id, outcome } => {
                self.running = self.running.saturating_sub(1);
                self.downloads.insert(id, outcome);
                self.prefetch();
                self.play_current();
            }
            AppEvent::Player { player, event } if player == self.players => self.on_player(event),
            AppEvent::Player { .. } => {}
        }
    }

    /// Adds a batch's videos not seen before, after the ones there.
    fn on_batch(&mut self, batch: Result<Batch, String>) {
        self.fetching = false;
        match batch {
            Ok(batch) => {
                let before = self.videos.len();
                for video in batch.videos {
                    if self.seen.insert(video.id.clone()) {
                        self.videos.push(video);
                    }
                }
                self.feed = FeedState::Fetched {
                    new: self.videos.len() - before,
                    engine: batch.engine,
                    took: batch.took,
                    note: batch.note,
                };
                self.failed_at = None;
            }
            Err(e) => {
                self.feed = FeedState::Failed(text::one_line(&e, 300));
                self.failed_at = Some(Instant::now());
            }
        }
    }

    fn on_player(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Restarted => self.playing = self.loaded.clone(),
            PlayerEvent::Position(seconds) => self.position = seconds,
            PlayerEvent::Duration(seconds) => self.duration = Some(seconds),
            PlayerEvent::Paused(paused) => self.paused = paused,
            PlayerEvent::Failed(e) => {
                self.playing = None;
                self.play_error = Some(format!("It can't play: {e}"));
            }
            PlayerEvent::Exited => {
                self.player = None;
                self.token = None;
                self.loaded = None;
                self.playing = None;
            }
        }
    }

    /// What would play for the video shown, if it can play yet: its file
    /// once downloaded, else its address streamed (too big to download, or
    /// the download failed).
    fn target(&self) -> Option<Target> {
        let video = self.current()?;
        match self.downloads.get(&video.id)? {
            Download::Ready(path) => Some(Target::File(path.clone())),
            Download::Streams | Download::Failed(_) => {
                let url = video.best(self.box_pixels())?.urls.first()?.clone();
                Some(Target::Stream {
                    url,
                    token: video.token.clone(),
                })
            }
            Download::Getting { .. } => None,
        }
    }

    /// Plays the video shown if it can and isn't already; while it can't
    /// yet, the player stops, so the last one doesn't play on under it.
    fn play_current(&mut self) {
        let Some(id) = self.current().map(|v| v.id.clone()) else {
            return;
        };
        if self.loaded.as_ref() == Some(&id) {
            return;
        }
        let Some(target) = self.target() else {
            self.stop_playing();
            return;
        };
        if !self.start_player() {
            return;
        }
        if let Some(player) = &self.player {
            player.load(&target);
        }
        self.loaded = Some(id);
        self.playing = None;
        self.position = 0.0;
        self.duration = None;
        self.paused = false;
        self.play_error = None;
    }

    fn stop_playing(&mut self) {
        if self.loaded.take().is_some()
            && let Some(player) = &self.player
        {
            player.stop();
        }
        self.playing = None;
    }

    /// Starts the player unless it runs; whether it runs.
    fn start_player(&mut self) -> bool {
        if self.player.is_some() {
            return true;
        }
        let Some((program, kind)) = self.player_with.clone() else {
            self.play_error = Some(NO_PLAYER.into());
            return false;
        };
        let Some(tx) = self.tx.clone() else {
            return false;
        };
        self.players += 1;
        let number = self.players;
        let said = move |event| {
            let _ = tx.send(AppEvent::Player {
                player: number,
                event,
            });
        };
        match Player::start(&program, &kind, said) {
            Ok(player) => {
                self.token = player.token.clone();
                self.player = Some(player);
                true
            }
            Err(e) => {
                self.play_error = Some(text::one_line(&format!("{e:#}"), 200));
                false
            }
        }
    }

    /// What to tell Pepper after a frame: the picture placed over the box
    /// while the video shown plays (placed again when the box moves), taken
    /// away otherwise. Empty when nothing changed, and always outside
    /// Pepper (no token).
    fn placement(&mut self) -> String {
        let want = self
            .token
            .clone()
            .filter(|_| self.shows_picture())
            .map(|token| Placed {
                token,
                area: self.video_area(),
            });
        if want == self.placed {
            return String::new();
        }
        let mut out = String::new();
        if let Some(old) = &self.placed
            && want.as_ref().is_none_or(|want| want.token != old.token)
        {
            out.push_str(&pepper::remove(&old.token));
        }
        if let Some(want) = &want {
            out.push_str(&pepper::place(&want.token, want.area));
        }
        self.placed = want;
        out
    }

    /// What to do so the video shown and the `cache::AHEAD` after it are
    /// downloaded, nearest first, `cache::PARALLEL` at a time: start a
    /// download, or mark a video that won't be downloaded (too big, or
    /// nothing in it plays).
    fn plan(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut running = self.running;
        let box_pixels = self.box_pixels();
        let end = (self.at + cache::AHEAD + 1).min(self.videos.len());
        for video in self.videos.get(self.at..end).unwrap_or_default() {
            if running >= cache::PARALLEL {
                break;
            }
            if self.downloads.contains_key(&video.id) {
                continue;
            }
            let id = video.id.clone();
            match video.best(box_pixels) {
                None => steps.push(Step::Mark(
                    id,
                    Download::Failed("nothing in it plays".into()),
                )),
                Some(r) if r.bytes.is_some_and(|bytes| bytes > cache::DOWNLOAD_UP_TO) => {
                    steps.push(Step::Mark(id, Download::Streams));
                }
                Some(r) => {
                    running += 1;
                    steps.push(Step::Start(cache::Job {
                        path: self.cache.join(cache::file_name(&id, r)),
                        id,
                        rendition: r.clone(),
                        token: video.token.clone(),
                    }));
                }
            }
        }
        steps
    }

    /// Carries out [`App::plan`].
    fn prefetch(&mut self) {
        let (Some(client), Some(tx)) = (self.client.clone(), self.tx.clone()) else {
            return;
        };
        for step in self.plan() {
            match step {
                Step::Mark(id, outcome) => {
                    self.downloads.insert(id, outcome);
                }
                Step::Start(job) => {
                    self.running += 1;
                    let getting = Download::Getting {
                        got: 0,
                        of: job.rendition.bytes,
                    };
                    self.downloads.insert(job.id.clone(), getting);
                    let (client, tx) = (client.clone(), tx.clone());
                    tokio::spawn(async move {
                        let progress = {
                            let (tx, id) = (tx.clone(), job.id.clone());
                            move |got, of| {
                                let _ = tx.send(AppEvent::Progress {
                                    id: id.clone(),
                                    got,
                                    of,
                                });
                            }
                        };
                        let outcome = cache::download(&client, &job, progress).await;
                        let _ = tx.send(AppEvent::Downloaded {
                            id: job.id,
                            outcome,
                        });
                    });
                }
            }
        }
    }

    /// Deletes the downloads more than `cache::BEHIND` before the video
    /// shown; going back that far downloads them again.
    fn prune(&mut self) {
        let keep_from = self.at.saturating_sub(cache::BEHIND);
        for video in &self.videos[..keep_from.min(self.videos.len())] {
            if matches!(self.downloads.get(&video.id), Some(Download::Ready(_)))
                && let Some(Download::Ready(path)) = self.downloads.remove(&video.id)
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Resize(columns, rows) => {
                self.cell = cell_size();
                self.size = (columns, rows);
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.go(1),
            KeyCode::Char('k') | KeyCode::Up => self.go(-1),
            KeyCode::Char(' ') => self.with_player(Player::toggle_pause),
            KeyCode::Char('m') => self.with_player(Player::toggle_mute),
            KeyCode::Char('h') | KeyCode::Left => self.with_player(|p| p.seek(-SEEK)),
            KeyCode::Char('l') | KeyCode::Right => self.with_player(|p| p.seek(SEEK)),
            KeyCode::Char('r') => {
                self.failed_at = None;
                self.fetch_more();
            }
            _ => {}
        }
    }

    /// `act` on the player, while it shows the video shown.
    fn with_player(&self, act: impl Fn(&Player)) {
        if let Some(player) = &self.player
            && self.shows_picture()
        {
            act(player);
        }
    }

    /// Shows the video `by` from this one, within the feed.
    fn go(&mut self, by: isize) {
        if self.videos.is_empty() {
            return;
        }
        self.at = self.at.saturating_add_signed(by).min(self.videos.len() - 1);
        self.prune();
        self.prefetch();
        self.play_current();
        self.refill();
    }

    /// Fetches more once few videos are left, unless a batch failed lately.
    fn refill(&mut self) {
        let left = self.videos.len().saturating_sub(self.at + 1);
        let resting = self.failed_at.is_some_and(|at| at.elapsed() < RETRY_AFTER);
        if left <= REFILL_AT && !resting {
            self.fetch_more();
        }
    }
}

/// Writes tuitok's own sequences to the terminal, after ratatui's frame.
fn write_to_terminal(text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// A cell's size in pixels: the terminal's size in pixels over its size in
/// cells (`TIOCGWINSZ`), else [`GUESSED_CELL`].
fn cell_size() -> (u16, u16) {
    match crossterm::terminal::window_size() {
        Ok(s) if s.columns > 0 && s.rows > 0 && s.width > 0 && s.height > 0 => {
            (s.width / s.columns, s.height / s.rows)
        }
        _ => GUESSED_CELL,
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::video::Rendition;

    pub fn app_with(videos: Vec<Video>) -> App {
        let session = Session {
            dir: PathBuf::from("/nowhere"),
            profile: PathBuf::from("/nowhere/chrome"),
            cache: PathBuf::from("/nowhere/cache"),
        };
        let mut app = App::new(false, Vec::new(), &session, None);
        app.cell = GUESSED_CELL;
        app.size = GUESSED_SIZE;
        app.on_batch(Ok(Batch {
            videos,
            token: None,
            engine: Engine::Lightpanda,
            took: Duration::from_secs(17),
            note: None,
        }));
        app
    }

    /// The player shows video `id`: in Pepper's box, or in mpv's window.
    pub fn set_playing(app: &mut App, id: &str, in_box: bool) {
        let id = VideoId::new(id).unwrap();
        app.loaded = Some(id.clone());
        app.playing = Some(id);
        if in_box {
            app.token = Some("t0k".into());
        } else {
            app.player_with = Some((PathBuf::from("/opt/homebrew/bin/mpv"), Kind::Window));
        }
    }

    #[test]
    fn batches_add_only_new_videos_and_keys_move_within_them() {
        let video = crate::ui::tests::video("1");
        let mut app = app_with(vec![video.clone(), crate::ui::tests::video("2")]);
        app.on_batch(Ok(Batch {
            videos: vec![video, crate::ui::tests::video("3")],
            token: None,
            engine: Engine::Chrome,
            took: Duration::from_secs(20),
            note: Some("Lightpanda got no videos".into()),
        }));
        assert_eq!(app.videos.len(), 3);
        assert!(matches!(
            app.feed,
            FeedState::Fetched {
                new: 1,
                engine: Engine::Chrome,
                ..
            }
        ));
        app.go(1);
        app.go(5);
        assert_eq!(app.at, 2);
        app.go(-9);
        assert_eq!(app.at, 0);
        app.on_batch(Err("Lightpanda: no\u{1b}[2J".into()));
        assert_eq!(app.feed, FeedState::Failed("Lightpanda: no[2J".into()));
    }

    #[test]
    fn downloads_are_planned_nearest_first_and_big_ones_stream() {
        let mut videos: Vec<Video> = (1..=6)
            .map(|n| crate::ui::tests::video(&n.to_string()))
            .collect();
        videos[1].renditions = vec![Rendition {
            bytes: Some(497_000_000),
            ..videos[1].renditions[0].clone()
        }];
        videos[3].renditions.clear();
        let mut app = app_with(videos);
        let plan = |app: &App| {
            app.plan()
                .into_iter()
                .map(|step| match step {
                    Step::Start(job) => format!("start {}", job.id.as_str()),
                    Step::Mark(id, Download::Streams) => format!("stream {}", id.as_str()),
                    Step::Mark(id, _) => format!("skip {}", id.as_str()),
                })
                .collect::<Vec<_>>()
        };
        // Two at a time: the first, the second marked to stream, the third.
        assert_eq!(plan(&app), ["start 1", "stream 2", "start 3"]);
        let job_path = match app.plan().remove(0) {
            Step::Start(job) => job.path,
            Step::Mark(..) => unreachable!(),
        };
        assert_eq!(
            job_path,
            PathBuf::from("/nowhere/cache/1-h265-720x1280.mp4")
        );
        // With those under way, the rest come as they finish.
        let started = Download::Getting { got: 0, of: None };
        app.downloads
            .insert(VideoId::new("1").unwrap(), started.clone());
        app.downloads
            .insert(VideoId::new("2").unwrap(), Download::Streams);
        app.downloads.insert(VideoId::new("3").unwrap(), started);
        app.running = 2;
        assert!(plan(&app).is_empty());
        app.on_app_event(AppEvent::Downloaded {
            id: VideoId::new("1").unwrap(),
            outcome: Download::Ready("/nowhere/cache/1.mp4".into()),
        });
        assert_eq!(plan(&app), ["skip 4"]);
        // Only the video shown and the three after it.
        app.at = 2;
        app.downloads
            .insert(VideoId::new("4").unwrap(), Download::Failed("x".into()));
        assert_eq!(plan(&app), ["start 5"]);
    }

    #[test]
    fn progress_updates_a_download_under_way() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        let id = VideoId::new("1").unwrap();
        app.downloads
            .insert(id.clone(), Download::Getting { got: 0, of: None });
        app.on_app_event(AppEvent::Progress {
            id: id.clone(),
            got: 300,
            of: Some(1_000),
        });
        assert_eq!(
            app.downloads[&id],
            Download::Getting {
                got: 300,
                of: Some(1_000)
            }
        );
    }

    #[test]
    fn what_plays_is_the_file_once_downloaded_else_the_stream() {
        let mut video = crate::ui::tests::video("1");
        video.token = Some(Arc::from("tok"));
        let mut app = app_with(vec![video]);
        let id = VideoId::new("1").unwrap();
        assert_eq!(app.target(), None);
        app.downloads
            .insert(id.clone(), Download::Getting { got: 1, of: None });
        assert_eq!(app.target(), None);
        app.downloads
            .insert(id.clone(), Download::Ready("/nowhere/cache/1.mp4".into()));
        assert_eq!(
            app.target(),
            Some(Target::File("/nowhere/cache/1.mp4".into()))
        );
        let stream = Some(Target::Stream {
            url: "https://v16-webapp-prime.tiktok.com/x".into(),
            token: Some(Arc::from("tok")),
        });
        app.downloads.insert(id.clone(), Download::Streams);
        assert_eq!(app.target(), stream);
        app.downloads
            .insert(id, Download::Failed("HTTP 403".into()));
        assert_eq!(app.target(), stream);
        // With no player to play with, it says so.
        app.play_current();
        assert_eq!(app.play_error.as_deref(), Some(NO_PLAYER));
    }

    #[test]
    fn pepper_is_told_where_the_picture_goes_only_when_it_changes() {
        let mut app = app_with(vec![
            crate::ui::tests::video("1"),
            crate::ui::tests::video("2"),
        ]);
        assert_eq!(app.placement(), "");
        set_playing(&mut app, "1", true);
        let area = app.video_area();
        assert_eq!(app.placement(), pepper::place("t0k", area));
        assert_eq!(app.placement(), "");
        // The window grew: placed again.
        app.size = (150, 50);
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area()));
        // Another video shown, not playing yet: taken away.
        app.at = 1;
        assert_eq!(app.placement(), pepper::remove("t0k"));
        assert_eq!(app.placement(), "");
    }

    #[test]
    fn the_players_words() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        app.loaded = Some(VideoId::new("1").unwrap());
        app.players = 2;
        app.on_app_event(AppEvent::Player {
            player: 2,
            event: PlayerEvent::Restarted,
        });
        assert!(app.shows_picture());
        app.on_app_event(AppEvent::Player {
            player: 2,
            event: PlayerEvent::Paused(true),
        });
        assert!(app.paused);
        // An older player's words are dropped.
        app.on_app_event(AppEvent::Player {
            player: 1,
            event: PlayerEvent::Exited,
        });
        assert!(app.shows_picture());
        app.on_app_event(AppEvent::Player {
            player: 2,
            event: PlayerEvent::Failed("loading failed".into()),
        });
        assert!(!app.shows_picture());
        assert_eq!(
            app.play_error.as_deref(),
            Some("It can't play: loading failed")
        );
        app.on_app_event(AppEvent::Player {
            player: 2,
            event: PlayerEvent::Exited,
        });
        assert!(app.loaded.is_none() && app.token.is_none());
    }
}
