//! What tuitok holds, and its event loop: one `tokio::select!` over the
//! terminal's events and tuitok's own (a batch of the feed, a download's
//! progress and end, what the player says), a redraw after each, and after
//! the frame, in Pepper, where the picture goes.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use image::DynamicImage;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::browser::Engine;
use crate::cache::{self, Download};
use crate::comments::{self, Comment, Comments};
use crate::config::Session;
use crate::covers::{self, Cover, Encoded};
use crate::demo;
use crate::feed::{self, Batch};
use crate::pepper::{self, Placed};
use crate::player::{Kind, Player, PlayerEvent, Sound, Target};
use crate::settings::{self, Settings};
use crate::text;
use crate::theme::{self, Colors};
use crate::ui;
use crate::video::{Video, VideoId};

/// A cell's size in pixels when the terminal doesn't say: about what most
/// fonts make, so the box is still about upright.
const GUESSED_CELL: (u16, u16) = (9, 18);

/// The terminal's size in cells when it doesn't say.
const GUESSED_SIZE: (u16, u16) = (120, 40);

/// Once the first batches (`feed::SIZES`, one after another) are in, the
/// next batch is fetched when the video shown is the 8th of the last one: 5
/// left after it.
const REFILL_AT: usize = feed::BATCH - 8;

/// After a batch fails, how long before one is tried again on its own (`r`
/// tries at once).
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Comments are fetched for the video shown and this many after it.
const COMMENTS_AHEAD: usize = 1;

/// How far `h` and `l` (or ← and →, `,` and `.`) seek, in seconds.
const SEEK: f64 = 5.0;

/// How far `-` and `+` turn the volume down or up, in percent (tuitube's).
const VOLUME_STEP: u32 = 5;

const NO_PLAYER: &str = "No player: brew install mpv";

/// What tuitok's own tasks tell the event loop.
pub enum AppEvent {
    /// Videos of the batch being fetched, as the page gets them.
    Found(Vec<Video>),
    /// The batch ended.
    Batch(Result<Batch, String>),
    /// How far a download has got.
    Progress {
        id: VideoId,
        got: u64,
        of: Option<u64>,
    },
    /// A download ended: ready, to stream, or failed.
    Downloaded { id: VideoId, outcome: Download },
    /// A failed download's wait is over: it's tried again
    /// ([`App::try_again_later`]).
    TryAgain(VideoId),
    /// A video's cover arrived and was decoded, or didn't.
    CoverFetched {
        id: VideoId,
        picture: Result<DynamicImage, String>,
    },
    /// A video's top comments came, or didn't.
    CommentsFetched {
        id: VideoId,
        comments: Result<Vec<Comment>, String>,
    },
    /// A cover was encoded for the terminal, or wasn't.
    CoverEncoded {
        id: VideoId,
        encoded: Result<Encoded, String>,
    },
    /// What player number `player` said: an older player's words are dropped.
    Player { player: u64, event: PlayerEvent },
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

/// The tabs of the `?` popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpTab {
    Keys,
    Settings,
}

/// The `?` popup, as tuitube's: every key, and the settings. On the
/// settings tab, Enter or Space picks the theme under the cursor and saves
/// it at once.
#[derive(Debug, PartialEq)]
pub struct HelpMenu {
    pub tab: HelpTab,
    /// The first line shown on the keys tab. Drawing keeps it in range.
    pub scroll: Cell<usize>,
    /// The row under the cursor on the settings tab, in `theme::names()`.
    pub selected: usize,
    /// Why the last theme picked couldn't be saved.
    pub error: Option<String>,
}

pub struct App {
    pub colors: Colors,
    /// The theme in use (its id in `theme::BUILT_IN`), the data folder
    /// it's saved in when picked (none in the demo and tests), and the `?`
    /// popup while it's open.
    pub theme: &'static str,
    settings_in: Option<PathBuf>,
    pub help: Option<HelpMenu>,
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
    /// What the last run's guest watched (`guest.rs`), and the furthest
    /// video shown in this run: handed to the next run.
    watched_before: Vec<VideoId>,
    furthest: usize,
    fetching: bool,
    /// Where the batch being fetched starts in `videos`, and how many it
    /// asks for.
    batch_from: usize,
    pub batch_size: usize,
    /// Batches fetched so far: the next one's size is `feed::batch_size`.
    batches: usize,
    /// Videos past what the last batch asked for: the next batch's first.
    spare: Vec<Video>,
    /// The guest's cookies from the last batch, for the next one: only in
    /// memory (`feed::Ask::cookies`).
    cookies: Vec<serde_json::Value>,
    failed_at: Option<Instant>,
    /// Downloads running.
    running: usize,
    /// How many times each video's download failed: it's tried again,
    /// `cache::TRIES` in all.
    failures: HashMap<VideoId, u32>,
    /// How loud, in percent (`-` and `+`), what settings.toml has (it's
    /// written when tuitok ends if that changed), and the sound off (`m`,
    /// for this run).
    pub volume: u32,
    volume_kept: u32,
    pub muted: bool,
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
    /// Covers by video, how the terminal draws pictures (none in tests),
    /// and the client for TikTok's image hosts.
    pub covers: HashMap<VideoId, Cover>,
    picker: Option<Picker>,
    cover_client: Option<reqwest::Client>,
    /// Top comments by video, and the client for `www.tiktok.com`'s
    /// comment list.
    pub comments: HashMap<VideoId, Comments>,
    comment_client: Option<reqwest::Client>,
    /// `f` in Pepper: the video fills the tab.
    pub fullscreen: bool,
    /// `--demo`: made-up videos, nothing fetched, downloaded or played.
    pub demo: bool,
    /// tuitok runs in Pepper but through a multiplexer, by name
    /// (`pepper::behind_multiplexer`): videos play in mpv's window.
    pub behind_multiplexer: Option<&'static str>,
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
            theme: theme::DEFAULT,
            settings_in: None,
            help: None,
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
            watched_before: Vec::new(),
            furthest: 0,
            fetching: false,
            batch_from: 0,
            batch_size: feed::batch_size(0),
            batches: 0,
            spare: Vec::new(),
            cookies: Vec::new(),
            failed_at: None,
            running: 0,
            failures: HashMap::new(),
            volume: 100,
            volume_kept: 100,
            muted: false,
            player_with,
            player: None,
            players: 0,
            loaded: None,
            token: None,
            placed: None,
            covers: HashMap::new(),
            picker: None,
            cover_client: None,
            comments: HashMap::new(),
            comment_client: None,
            fullscreen: false,
            demo: false,
            behind_multiplexer: None,
            client: None,
            tx: None,
            quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let (tx, mut app_events) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        if !self.demo {
            self.client = Some(cache::client(crate::video::video_url_allowed));
            self.cover_client = Some(cache::client(crate::video::image_url_allowed));
            self.comment_client = Some(cache::client(comments::url_allowed));
        }
        self.want_covers();
        self.want_comments();
        self.fetch_more();
        let mut terminal_events = EventStream::new();
        // The terminal window closed (SIGHUP), or tuitok was asked to end
        // (SIGTERM): it quits as with q, so the player quits, Pepper's box
        // goes and the session's folder is deleted.
        let mut hangup = signal(SignalKind::hangup())?;
        let mut terminate = signal(SignalKind::terminate())?;
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
                _ = hangup.recv() => self.quit = true,
                _ = terminate.recv() => self.quit = true,
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

    /// Where things go, laid out as `ui::draw` lays them out.
    fn areas(&self) -> ui::Areas {
        ui::areas(
            Rect::new(0, 0, self.size.0, self.size.1),
            self.cell,
            self.fullscreen,
        )
    }

    /// The video box.
    fn video_area(&self) -> Rect {
        self.areas().video
    }

    /// The video box's size in pixels.
    pub fn box_pixels(&self) -> (u32, u32) {
        let video = self.video_area();
        (
            u32::from(video.width) * u32::from(self.cell.0),
            u32::from(video.height) * u32::from(self.cell.1),
        )
    }

    /// The cover's size in cells, as `ui` draws it in the box.
    fn cover_size(&self) -> (u16, u16) {
        let area = ui::cover_area(self.video_area(), self.demo);
        (area.width, area.height)
    }

    /// Whether the terminal paints each picture over what's drawn on it
    /// later (sixel, iTerm2's protocol), so none may go under a popup.
    pub fn pictures_paint_over(&self) -> bool {
        self.picker.as_ref().is_some_and(|picker| {
            matches!(
                picker.protocol_type(),
                ProtocolType::Sixel | ProtocolType::Iterm2
            )
        })
    }

    /// Sets how pictures are drawn, and takes the cell size it found if the
    /// terminal didn't say (a picker that didn't ask guesses 10×20).
    pub fn set_picker(&mut self, picker: Picker) {
        let font = picker.font_size();
        if self.cell == GUESSED_CELL && font.width > 0 && font.height > 0 {
            self.cell = (font.width, font.height);
        }
        self.picker = Some(picker);
    }

    /// `--demo`: made-up videos and covers drawn in code; nothing is
    /// fetched, downloaded or played.
    pub fn demo(pepper: bool) -> Self {
        let nowhere = Session {
            data: PathBuf::new(),
            dir: PathBuf::new(),
            profile: PathBuf::new(),
            cache: PathBuf::new(),
        };
        let mut app = Self::new(pepper, Vec::new(), &nowhere, None);
        app.demo = true;
        let videos = demo::videos();
        for (n, video) in videos.iter().enumerate() {
            let cover = Cover::Ready {
                picture: Arc::new(demo::cover(n)),
                encoded: None,
                encoding: false,
            };
            app.covers.insert(video.id.clone(), cover);
            let made_up = Comments::Ready(demo::comments(n));
            app.comments.insert(video.id.clone(), made_up);
        }
        app.on_batch(Ok(Batch {
            videos,
            token: None,
            engine: Engine::Lightpanda,
            took: Duration::from_secs(14),
            first: Some(Duration::from_millis(1500)),
            spare: Vec::new(),
            cookies: Vec::new(),
            note: None,
        }));
        app
    }

    /// Goes on from the last run's guest: its cookies for the first batch,
    /// and what it watched is skipped if TikTok sends it again.
    pub fn carry_in(&mut self, guest: crate::guest::Guest) {
        self.cookies = guest.cookies;
        self.seen.extend(guest.watched.iter().cloned());
        self.watched_before = guest.watched;
    }

    /// The guest to hand the next run: the cookies of the last batch (or
    /// the last run's, if none came), and what was watched, this run's
    /// after the last run's.
    pub fn carry_out(&self) -> crate::guest::Guest {
        let shown = self.videos.len().min(self.furthest + 1);
        let mut watched = self.watched_before.clone();
        watched.extend(self.videos[..shown].iter().map(|v| v.id.clone()));
        crate::guest::Guest {
            cookies: self.cookies.clone(),
            watched,
        }
    }

    /// The settings read at the start, and the data folder to save them in
    /// (none: what's picked is kept for this run only).
    pub fn use_settings(&mut self, settings: Settings, save_in: Option<PathBuf>) {
        self.set_theme(settings.theme);
        self.volume = settings.volume;
        self.volume_kept = settings.volume;
        self.settings_in = save_in;
    }

    fn set_theme(&mut self, id: &'static str) {
        self.theme = id;
        self.colors = Colors::named(id);
    }

    /// How many videos the batch being fetched has added so far.
    pub fn found_so_far(&self) -> usize {
        self.videos.len() - self.batch_from
    }

    /// Whether the player shows the video shown.
    pub fn shows_picture(&self) -> bool {
        self.playing.is_some() && self.playing.as_ref() == self.current().map(|v| &v.id)
    }

    /// Whether that picture is in Pepper's box (not in mpv's window): not
    /// while the `?` popup is open, which it would cover (as in tuitube).
    pub fn picture_in_box(&self) -> bool {
        self.shows_picture() && self.token.is_some() && self.help.is_none()
    }

    /// Whether the player was told to play the video shown and has no
    /// frame of it yet.
    pub fn loading(&self) -> bool {
        self.loaded.is_some()
            && self.loaded.as_ref() == self.current().map(|v| &v.id)
            && !self.shows_picture()
    }

    /// Starts fetching the next batch, unless one is being fetched.
    fn fetch_more(&mut self) {
        if self.fetching {
            return;
        }
        let (Some(tx), Some(&(engine, _))) = (self.tx.clone(), self.engines.first()) else {
            return;
        };
        let Some(ask) = self.next_ask() else {
            return;
        };
        self.fetching = true;
        self.feed = FeedState::Fetching(engine);
        let (engines, profile) = (self.engines.clone(), self.profile.clone());
        tokio::spawn(async move {
            let tell = |videos| {
                let _ = tx.send(AppEvent::Found(videos));
            };
            let batch = feed::fetch_any(engines, profile, ask, &tell)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Batch(batch));
        });
    }

    /// What the next batch asks the browser for. The spare videos come
    /// first: a batch they fill needs no browser, and the next one starts
    /// while the first batches are coming in. `None` once nothing's to fetch.
    fn next_ask(&mut self) -> Option<feed::Ask> {
        loop {
            self.batch_size = feed::batch_size(self.batches);
            self.batch_from = self.videos.len();
            let take = self.spare.len().min(self.batch_size);
            let spare: Vec<Video> = self.spare.drain(..take).collect();
            if !spare.is_empty() {
                self.add(spare);
                self.prefetch();
                self.want_covers();
                self.want_comments();
                self.play_current();
            }
            if self.found_so_far() < self.batch_size {
                return Some(feed::Ask {
                    want: self.batch_size - self.found_so_far(),
                    seen: self.seen.clone(),
                    cookies: self.cookies.clone(),
                });
            }
            self.batches += 1;
            self.batch_from = self.videos.len();
            if !self.ramping() {
                return None;
            }
        }
    }

    /// Whether the next batch is one of the first, fetched as soon as the
    /// one before ends.
    fn ramping(&self) -> bool {
        self.batches < feed::SIZES.len()
    }

    fn on_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Found(videos) => {
                self.add(videos);
                self.prefetch();
                self.want_covers();
                self.want_comments();
                self.play_current();
            }
            AppEvent::Batch(batch) => {
                self.on_batch(batch);
                self.prefetch();
                self.want_covers();
                self.want_comments();
                self.play_current();
            }
            AppEvent::CommentsFetched { id, comments } => {
                let comments = comments.map_or(Comments::Failed, Comments::Ready);
                self.comments.insert(id, comments);
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
                if matches!(outcome, Download::Failed(_)) {
                    self.try_again_later(&id);
                }
                self.downloads.insert(id, outcome);
                self.prefetch();
                self.play_current();
            }
            AppEvent::TryAgain(id) => self.try_again(&id),
            AppEvent::CoverFetched { id, picture } => {
                let cover = match picture {
                    Ok(picture) => Cover::Ready {
                        picture: Arc::new(picture),
                        encoded: None,
                        encoding: false,
                    },
                    Err(_) => Cover::Failed,
                };
                self.covers.insert(id, cover);
                self.want_covers();
            }
            AppEvent::CoverEncoded { id, encoded } => {
                let failed = match (self.covers.get_mut(&id), encoded) {
                    (
                        Some(Cover::Ready {
                            encoded: slot,
                            encoding,
                            ..
                        }),
                        Ok(encoded),
                    ) => {
                        *encoding = false;
                        *slot = Some(encoded);
                        false
                    }
                    (Some(Cover::Ready { .. }), Err(_)) => true,
                    _ => false,
                };
                if failed {
                    self.covers.insert(id, Cover::Failed);
                }
                self.want_covers();
            }
            AppEvent::Player { player, event } if player == self.players => self.on_player(event),
            AppEvent::Player { .. } => {}
        }
    }

    /// Adds the videos not seen before, after the ones there.
    fn add(&mut self, videos: Vec<Video>) {
        for video in videos {
            if self.seen.insert(video.id.clone()) {
                self.videos.push(video);
            }
        }
    }

    /// A batch ended: its videos came before (`AppEvent::Found`), any not
    /// added yet are. While the first batches are in, the next starts.
    fn on_batch(&mut self, batch: Result<Batch, String>) {
        self.fetching = false;
        let ok = batch.is_ok();
        match batch {
            Ok(batch) => {
                self.add(batch.videos);
                self.spare.extend(batch.spare);
                if !batch.cookies.is_empty() {
                    self.cookies = batch.cookies;
                }
                self.batches += 1;
                self.feed = FeedState::Fetched {
                    new: self.found_so_far(),
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
        self.batch_from = self.videos.len();
        if ok && self.ramping() {
            self.fetch_more();
        }
    }

    fn on_player(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Restarted => self.playing = self.loaded.clone(),
            PlayerEvent::Position(seconds) => self.position = seconds,
            PlayerEvent::Duration(seconds) => self.duration = Some(seconds),
            PlayerEvent::Paused(paused) => self.paused = paused,
            // Changed in mpv's window too: the next video plays as loud.
            PlayerEvent::Volume(volume) => self.volume = volume.round().clamp(0.0, 100.0) as u32,
            PlayerEvent::Muted(muted) => self.muted = muted,
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
        if self.demo {
            return;
        }
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
        let sound = Sound {
            volume: self.volume,
            muted: self.muted,
        };
        match Player::start(&program, &kind, sound, said) {
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
            .filter(|_| self.picture_in_box())
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

    /// A download failed: it's tried again after `cache::retry_after`,
    /// `cache::TRIES` in all. Meanwhile the video streams if it's shown.
    fn try_again_later(&mut self, id: &VideoId) {
        let failed = {
            let failed = self.failures.entry(id.clone()).or_default();
            *failed += 1;
            *failed
        };
        let (Some(wait), Some(tx)) = (cache::retry_after(failed), self.tx.clone()) else {
            return;
        };
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = tx.send(AppEvent::TryAgain(id));
        });
    }

    /// Downloads a video again once its wait is over, unless it plays
    /// streamed meanwhile (it's shown), or its download isn't failed any
    /// more (pruned, or downloaded again on the way back to it).
    fn try_again(&mut self, id: &VideoId) {
        if self.loaded.as_ref() == Some(id)
            || !matches!(self.downloads.get(id), Some(Download::Failed(_)))
        {
            return;
        }
        self.downloads.remove(id);
        self.prefetch();
        self.play_current();
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
        // Covers too, but not the demo's, which can't be fetched again.
        if self.demo {
            return;
        }
        let keep_to = self.at + cache::AHEAD;
        for (n, video) in self.videos.iter().enumerate() {
            if (n < keep_from || n > keep_to)
                && !matches!(self.covers.get(&video.id), Some(Cover::Loading))
            {
                self.covers.remove(&video.id);
            }
            if (n < keep_from || n > keep_to)
                && !matches!(self.comments.get(&video.id), Some(Comments::Loading))
            {
                self.comments.remove(&video.id);
            }
        }
    }

    /// Fetches the top comments of the video shown and the
    /// `COMMENTS_AHEAD` after it that aren't asked for yet.
    fn want_comments(&mut self) {
        let (Some(client), Some(tx)) = (self.comment_client.clone(), self.tx.clone()) else {
            return;
        };
        let end = (self.at + COMMENTS_AHEAD + 1).min(self.videos.len());
        for video in self.videos.get(self.at..end).unwrap_or_default() {
            if self.comments.contains_key(&video.id) {
                continue;
            }
            self.comments.insert(video.id.clone(), Comments::Loading);
            let (client, tx, id) = (client.clone(), tx.clone(), video.id.clone());
            tokio::spawn(async move {
                let comments = comments::fetch(&client, &id)
                    .await
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(AppEvent::CommentsFetched { id, comments });
            });
        }
    }

    /// Fetches the covers of the video shown and the `cache::AHEAD` after
    /// it that aren't asked for yet, and encodes the fetched ones at the
    /// cover's size (again when that changes).
    fn want_covers(&mut self) {
        let (Some(picker), Some(tx)) = (self.picker.clone(), self.tx.clone()) else {
            return;
        };
        let size = self.cover_size();
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        let end = (self.at + cache::AHEAD + 1).min(self.videos.len());
        for video in self.videos.get(self.at..end).unwrap_or_default() {
            match self.covers.get_mut(&video.id) {
                None => {
                    let (Some(client), Some(url)) =
                        (self.cover_client.clone(), video.cover.clone())
                    else {
                        continue;
                    };
                    self.covers.insert(video.id.clone(), Cover::Loading);
                    let (tx, id) = (tx.clone(), video.id.clone());
                    tokio::spawn(async move {
                        let picture = covers::fetch(&client, &url)
                            .await
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(AppEvent::CoverFetched { id, picture });
                    });
                }
                Some(Cover::Ready {
                    picture,
                    encoded,
                    encoding,
                }) if !*encoding && encoded.as_ref().is_none_or(|e| e.size != size) => {
                    *encoding = true;
                    let (tx, id, picker, picture) = (
                        tx.clone(),
                        video.id.clone(),
                        picker.clone(),
                        picture.clone(),
                    );
                    tokio::spawn(async move {
                        let encoded = covers::encode(picker, picture, size)
                            .await
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(AppEvent::CoverEncoded { id, encoded });
                    });
                }
                _ => {}
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Resize(columns, rows) => {
                self.cell = cell_size();
                self.size = (columns, rows);
                self.want_covers();
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.help.is_some() {
            self.on_help_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.go(1),
            KeyCode::Char('k') | KeyCode::Up => self.go(-1),
            KeyCode::Char(' ') => self.with_player(Player::toggle_pause),
            KeyCode::Char('m') => self.set_muted(!self.muted),
            KeyCode::Char('-') => self.set_volume(self.volume.saturating_sub(VOLUME_STEP)),
            KeyCode::Char('+' | '=') => self.set_volume(self.volume + VOLUME_STEP),
            KeyCode::Char('h' | ',') | KeyCode::Left => self.with_player(|p| p.seek(-SEEK)),
            KeyCode::Char('l' | '.') | KeyCode::Right => self.with_player(|p| p.seek(SEEK)),
            KeyCode::Char('f') => self.toggle_fullscreen(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('r') => {
                self.failed_at = None;
                self.fetch_more();
            }
            _ => {}
        }
    }

    /// `?`: the popup, on its keys tab, the settings' cursor on the theme in
    /// use.
    fn open_help(&mut self) {
        let selected = theme::names()
            .iter()
            .position(|(id, _)| *id == self.theme)
            .unwrap_or(0);
        self.help = Some(HelpMenu {
            tab: HelpTab::Keys,
            scroll: Cell::new(0),
            selected,
            error: None,
        });
    }

    /// Keys while the `?` popup is open, as tuitube's: Tab (or `h` `l`, ←
    /// →) the other tab, `j` `k` scroll the keys or move the settings'
    /// cursor, Enter or Space pick the theme under it, Esc, `q` or `?` close.
    fn on_help_key(&mut self, key: KeyEvent) {
        let Some(menu) = self.help.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('h' | 'l')
            | KeyCode::Left
            | KeyCode::Right => {
                menu.tab = match menu.tab {
                    HelpTab::Keys => HelpTab::Settings,
                    HelpTab::Settings => HelpTab::Keys,
                };
                return;
            }
            KeyCode::Enter | KeyCode::Char(' ') if menu.tab == HelpTab::Settings => {
                let row = menu.selected;
                self.pick_theme(row);
                return;
            }
            KeyCode::Esc | KeyCode::Char('q' | '?') => {
                self.help = None;
                return;
            }
            _ => {}
        }
        let delta: isize = match key.code {
            KeyCode::Char('j') | KeyCode::Down => 1,
            KeyCode::Char('k') | KeyCode::Up => -1,
            KeyCode::Char('g') | KeyCode::Home => isize::MIN,
            KeyCode::Char('G') | KeyCode::End => isize::MAX,
            _ => return,
        };
        match menu.tab {
            // Drawing stops it at the end of the list.
            HelpTab::Keys => menu
                .scroll
                .set(menu.scroll.get().saturating_add_signed(delta)),
            HelpTab::Settings => {
                let last = theme::names().len() - 1;
                menu.selected = menu.selected.saturating_add_signed(delta).min(last);
            }
        }
    }

    /// Enter or Space on the settings tab: the theme on `row` in use, and
    /// saved at once; why not, if it can't be.
    fn pick_theme(&mut self, row: usize) {
        let Some((id, _)) = theme::names().get(row) else {
            return;
        };
        self.set_theme(id);
        let saved = self.save_theme();
        if let Some(menu) = &mut self.help {
            menu.error = saved.err();
        }
    }

    /// `-` and `+`: how loud, in percent, what plays and what plays next,
    /// and the sound on again, as YouTube's player does (tuitube's way).
    fn set_volume(&mut self, volume: u32) {
        self.volume = volume.min(100);
        self.muted = false;
        if let Some(player) = &self.player {
            player.set_volume(self.volume);
            player.set_muted(false);
        }
    }

    /// `m`: the sound off, or on again, for this run.
    fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Some(player) = &self.player {
            player.set_muted(muted);
        }
    }

    /// When tuitok ends: the volume kept in settings.toml if it changed in
    /// this run (not in the demo).
    pub fn keep_volume(&self) -> anyhow::Result<()> {
        match &self.settings_in {
            Some(data) if self.volume != self.volume_kept => {
                settings::save(data, "volume", i64::from(self.volume).into())
            }
            _ => Ok(()),
        }
    }

    /// Keeps the theme in use in settings.toml, if there's a folder for it.
    fn save_theme(&self) -> Result<(), String> {
        let Some(data) = &self.settings_in else {
            return Ok(());
        };
        settings::save(data, "theme", self.theme.into())
            .map_err(|e| text::one_line(&format!("{e:#}"), 200))
    }

    /// `act` on the player, while it shows the video shown.
    fn with_player(&self, act: impl Fn(&Player)) {
        if let Some(player) = &self.player
            && self.shows_picture()
        {
            act(player);
        }
    }

    /// `f`: in Pepper the video fills the tab, or goes back to its box;
    /// elsewhere mpv's window goes full screen.
    fn toggle_fullscreen(&mut self) {
        if self.pepper || self.demo {
            self.fullscreen = !self.fullscreen;
            self.want_covers();
        } else {
            self.with_player(Player::toggle_fullscreen);
        }
    }

    /// Shows the video `by` from this one, within the feed.
    fn go(&mut self, by: isize) {
        if self.videos.is_empty() {
            return;
        }
        self.at = self.at.saturating_add_signed(by).min(self.videos.len() - 1);
        self.furthest = self.furthest.max(self.at);
        self.prune();
        self.prefetch();
        self.want_covers();
        self.want_comments();
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
            data: PathBuf::from("/nowhere"),
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
            first: None,
            spare: Vec::new(),
            cookies: Vec::new(),
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
            first: None,
            spare: Vec::new(),
            cookies: Vec::new(),
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

    fn batch(videos: &[&str], spare: &[&str]) -> Batch {
        Batch {
            videos: videos
                .iter()
                .map(|id| crate::ui::tests::video(id))
                .collect(),
            spare: spare.iter().map(|id| crate::ui::tests::video(id)).collect(),
            cookies: vec![serde_json::json!({"name": "ttwid", "value": "a"})],
            token: None,
            engine: Engine::Lightpanda,
            took: Duration::from_secs(2),
            first: None,
            note: None,
        }
    }

    #[test]
    fn batches_grow_and_the_spare_comes_first() {
        let mut app = app_with(Vec::new());
        app.batches = 0;
        assert_eq!(app.next_ask().unwrap().want, 1);
        // The page's answer had 2: one is spare, the first of the next batch.
        app.on_batch(Ok(batch(&["1"], &["2"])));
        let ask = app.next_ask().unwrap();
        assert_eq!((app.batch_size, ask.want, app.found_so_far()), (2, 1, 1));
        assert!(ask.seen.contains(&VideoId::new("2").unwrap()));
        assert_eq!(ask.cookies.len(), 1, "the guest goes on");
        // A spare that fills the batches of 3 and 5 needs no browser for them.
        app.on_batch(Ok(batch(
            &["3"],
            &["4", "5", "6", "7", "8", "9", "10", "11"],
        )));
        assert_eq!(app.videos.len(), 3);
        let ask = app.next_ask().unwrap();
        assert_eq!((app.batches, app.batch_size, ask.want), (4, 8, 8));
        assert_eq!(app.videos.len(), 11);
    }

    #[test]
    fn the_last_runs_guest_goes_on_and_what_it_watched_is_skipped() {
        let mut app = app_with(Vec::new());
        app.batches = 0;
        let cookie = serde_json::json!({"name": "ttwid", "value": "a", "domain": ".tiktok.com"});
        app.carry_in(crate::guest::Guest {
            cookies: vec![cookie.clone()],
            watched: vec![VideoId::new("1").unwrap()],
        });
        let ask = app.next_ask().unwrap();
        assert_eq!(ask.cookies, [cookie]);
        assert!(ask.seen.contains(&VideoId::new("1").unwrap()));
        // TikTok sends 1 again: it isn't shown.
        app.on_app_event(AppEvent::Found(
            ["1", "2", "3", "4"].map(crate::ui::tests::video).to_vec(),
        ));
        assert_eq!(app.videos.len(), 3);
        app.go(1);
        let out = app.carry_out();
        let ids: Vec<&str> = out.watched.iter().map(VideoId::as_str).collect();
        assert_eq!(ids, ["1", "2", "3"], "4 wasn't shown");
        assert_eq!(out.cookies.len(), 1);
    }

    #[test]
    fn after_the_first_batches_a_full_spare_waits_for_the_8th_video() {
        let mut app = app_with(Vec::new());
        app.batches = feed::SIZES.len();
        app.spare = (1..=15)
            .map(|n| crate::ui::tests::video(&n.to_string()))
            .collect();
        assert!(app.next_ask().is_none());
        assert_eq!((app.videos.len(), app.spare.len()), (13, 2));
        let ask = app.next_ask().unwrap();
        assert_eq!((app.found_so_far(), ask.want), (2, 11));
        assert_eq!(REFILL_AT, 13 - 8);
    }

    #[test]
    fn videos_count_as_they_come_and_the_batch_counts_them_once() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        app.fetching = true;
        app.on_app_event(AppEvent::Found(vec![crate::ui::tests::video("2")]));
        app.on_app_event(AppEvent::Found(vec![
            crate::ui::tests::video("2"),
            crate::ui::tests::video("3"),
        ]));
        assert_eq!(app.videos.len(), 3);
        assert_eq!(app.found_so_far(), 2);
        let all = ["1", "2", "3", "4"].map(crate::ui::tests::video).to_vec();
        app.on_app_event(AppEvent::Batch(Ok(Batch {
            videos: all,
            token: None,
            engine: Engine::Lightpanda,
            took: Duration::from_secs(14),
            first: Some(Duration::from_millis(1500)),
            spare: Vec::new(),
            cookies: Vec::new(),
            note: None,
        })));
        assert_eq!(app.videos.len(), 4);
        assert!(matches!(app.feed, FeedState::Fetched { new: 3, .. }));
        assert_eq!(app.found_so_far(), 0);
        assert!(!app.fetching);
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

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn a_failed_download_is_tried_again_unless_it_streams_meanwhile() {
        let mut app = app_with(vec![
            crate::ui::tests::video("1"),
            crate::ui::tests::video("2"),
        ]);
        let (one, two) = (VideoId::new("1").unwrap(), VideoId::new("2").unwrap());
        let failed = |id: &VideoId| AppEvent::Downloaded {
            id: id.clone(),
            outcome: Download::Failed("HTTP 503".into()),
        };
        app.downloads
            .insert(one.clone(), Download::Ready("/nowhere/cache/1.mp4".into()));
        app.on_app_event(failed(&two));
        assert_eq!(app.failures[&two], 1);
        // Its wait is over: downloaded again.
        app.on_app_event(AppEvent::TryAgain(two.clone()));
        assert!(!app.downloads.contains_key(&two));
        assert!(matches!(&app.plan()[..], [Step::Start(job)] if job.id == two));
        app.on_app_event(failed(&two));
        assert_eq!(app.failures[&two], 2);
        // Shown and streamed meanwhile: not downloaded as well.
        app.loaded = Some(two.clone());
        app.on_app_event(AppEvent::TryAgain(two.clone()));
        assert!(matches!(app.downloads[&two], Download::Failed(_)));
        // One that isn't failed any more is left as it is.
        app.on_app_event(AppEvent::TryAgain(one.clone()));
        assert!(matches!(app.downloads[&one], Download::Ready(_)));
    }

    #[test]
    fn question_mark_opens_the_keys_and_the_settings_pick_a_theme_at_once() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/test-app-settings-{}", std::process::id()));
        crate::config::private_dir(&data).unwrap();
        let nord = Settings {
            theme: "nord",
            ..Settings::default()
        };
        app.use_settings(nord, Some(data.clone()));
        assert_eq!(app.colors, Colors::named("nord"));
        press(&mut app, KeyCode::Char('?'));
        let menu = app.help.as_ref().unwrap();
        assert_eq!(menu.tab, HelpTab::Keys);
        assert_eq!(
            theme::names()[menu.selected].0,
            "nord",
            "on the theme in use"
        );
        // While it's open, the keys are its own.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('f'));
        assert!(!app.fullscreen && app.at == 0);
        assert_eq!(app.help.as_ref().unwrap().scroll.get(), 1);
        // Enter on the keys tab does nothing; on the settings tab it picks
        // the theme under the cursor and saves it, and the popup stays.
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.theme, "nord");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Enter);
        let before_nord = theme::names()[theme::names().len() - 3].0;
        assert_eq!(app.help.as_ref().unwrap().tab, HelpTab::Settings);
        assert_eq!(
            (app.theme, settings::load(&data).theme),
            (before_nord, before_nord)
        );
        assert_eq!(app.colors, Colors::named(before_nord));
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(
            app.help.as_ref().unwrap().selected,
            theme::names().len() - 1
        );
        // q closes it, and doesn't quit; it kept what was picked.
        press(&mut app, KeyCode::Char('q'));
        assert!(app.help.is_none() && !app.quit);
        assert_eq!(app.theme, before_nord);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn the_volume_goes_by_5_unmutes_and_is_kept_when_it_changed() {
        let mut app = app_with(Vec::new());
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/test-app-volume-{}", std::process::id()));
        crate::config::private_dir(&data).unwrap();
        let at = |volume| Settings {
            theme: theme::DEFAULT,
            volume,
        };
        app.use_settings(at(100), Some(data.clone()));
        // Unchanged: nothing written.
        app.keep_volume().unwrap();
        assert!(!data.join("settings.toml").exists());
        press(&mut app, KeyCode::Char('+'));
        assert_eq!(app.volume, 100, "no louder than 100");
        press(&mut app, KeyCode::Char('m'));
        assert!(app.muted);
        press(&mut app, KeyCode::Char('-'));
        press(&mut app, KeyCode::Char('-'));
        assert_eq!((app.volume, app.muted), (90, false));
        press(&mut app, KeyCode::Char('='));
        assert_eq!(app.volume, 95);
        app.keep_volume().unwrap();
        assert_eq!(settings::load(&data).volume, 95);
        // Changed in mpv's window: followed, within 0 to 100.
        app.players = 1;
        for (said, volume) in [(42.4, 42), (130.0, 100)] {
            app.on_app_event(AppEvent::Player {
                player: 1,
                event: PlayerEvent::Volume(said),
            });
            assert_eq!(app.volume, volume);
        }
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn the_popup_takes_the_picture_away() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        set_playing(&mut app, "1", true);
        let area = app.video_area();
        assert_eq!(app.placement(), pepper::place("t0k", area));
        press(&mut app, KeyCode::Char('?'));
        assert!(!app.picture_in_box());
        assert_eq!(app.placement(), pepper::remove("t0k"));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area()));
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
