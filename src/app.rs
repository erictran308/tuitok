//! What tuitok holds, and its event loop: one `tokio::select!` over the
//! terminal's events and tuitok's own (a batch of the feed, a download's
//! progress and end, what the player says), a redraw after each, and after
//! the frame, in Pepper, where the picture goes.

use std::cell::{Cell, RefCell};
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
use crate::explore::{self, Category};
use crate::feed::{self, Batch};
use crate::links::{self, Link};
use crate::pepper::{self, Placed};
use crate::player::{Kind, Player, PlayerEvent, Sound, Target};
use crate::settings::{self, Settings};
use crate::text;
use crate::theme::{self, Colors};
use crate::typed::Typed;
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

/// After a fetch fails, or brings nothing new, how long before one is tried
/// again on its own (`r` tries at once).
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// How long the video shown waits for its download before it streams
/// instead (mpv's first frame streamed: 0.2 to 0.9 s).
const STREAM_AFTER: Duration = Duration::from_secs(1);

/// How long the controls show over Pepper's full screen after anything's
/// done (tuitube's `CONTROLS_FOR`).
const CONTROLS_FOR: Duration = Duration::from_secs(3);

/// The most videos kept before the one shown: older ones count as watched
/// and are forgotten (their ids stay in `seen`, so they aren't shown
/// again), so a run of hours doesn't hold every video it showed.
const KEPT_BEHIND: usize = 100;

/// Comments are fetched for the video shown and this many after it.
const COMMENTS_AHEAD: usize = 1;

/// How far `h` and `l` (or ← and →, `,` and `.`) seek, in seconds.
const SEEK: f64 = 5.0;

/// How far `-` and `+` turn the volume down or up, in percent (tuitube's).
const VOLUME_STEP: u32 = 5;

const NO_PLAYER: &str = "No player: brew install mpv";

/// The most characters the command line (`/`) takes.
const MAX_SEARCH: usize = 40;

/// The most characters the command line `:` takes: `open` and the longest
/// link read.
const MAX_COMMAND: usize = links::MAX_LINK + 16;

/// What tuitok's own tasks tell the event loop.
pub enum AppEvent {
    /// Videos of the batch being fetched, as the page gets them.
    Found(Vec<Video>),
    /// The batch ended.
    Batch(Result<Batch, String>),
    /// A page of a category's videos came, or didn't.
    Explored {
        category: Category,
        page: Result<explore::Page, String>,
    },
    /// The video link `number` of the run is to came, or didn't (an older
    /// link's is dropped).
    Opened {
        number: u64,
        video: Result<Video, String>,
    },
    /// How far download `number` of a video has got.
    Progress {
        id: VideoId,
        number: u64,
        got: u64,
        of: Option<u64>,
    },
    /// Download `number` of a video ended: ready, to stream, or failed.
    Downloaded {
        id: VideoId,
        number: u64,
        outcome: Download,
    },
    /// A failed download's wait is over: it's tried again
    /// ([`App::try_again_later`]).
    TryAgain(VideoId),
    /// The video shown has waited `STREAM_AFTER` for its download
    /// ([`App::wait_to_stream`]).
    Waited(VideoId),
    /// A failed fetch's wait is over ([`App::rest`]).
    Rested,
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
    /// The device playing the sound went (headphones unplugged): its name,
    /// as the system calls it. Pause, as an iPhone does (`crate::output`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    OutputGone(String),
    /// tuitok can't see the headphones go: why.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    OutputUnwatched(String),
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
    /// A page of a category being fetched (no browser: `explore`).
    Exploring,
    /// The category's last page: the videos it added, and how long it took.
    Explored {
        new: usize,
        took: Duration,
    },
    Failed(String),
}

/// What For You's status is before its first batch: fetching with the
/// first of `engines`, or nothing to fetch with.
fn first_state(engines: &[(Engine, PathBuf)]) -> FeedState {
    match engines.first() {
        Some(&(engine, _)) => FeedState::Fetching(engine),
        None => FeedState::NoBrowser,
    }
}

/// A step towards having the next videos downloaded ([`App::plan`]).
enum Step {
    /// A download, by its number in the run.
    Start(u64, cache::Job),
    Mark(VideoId, Download),
}

/// A download under way: its number in the run, and how to stop it.
struct Underway {
    number: u64,
    abort: tokio::task::AbortHandle,
}

/// Whether tuitok watches the sound's device, to pause when the headphones
/// are unplugged (`crate::output`), as tuitube does.
#[derive(Debug, PartialEq)]
pub enum OutputWatch {
    /// Never: in tests and the demo.
    Never,
    /// From the first video played with `pause_on_unplug` on (`main`).
    Later,
    Watching,
    /// CoreAudio couldn't be asked: why.
    Failed(String),
}

/// The tabs of the `?` popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpTab {
    Keys,
    Settings,
}

/// The `?` popup, as tuitube's: every key, and the settings. On the
/// settings tab, Enter or Space turns the setting under the cursor on or
/// off, or picks the theme under it, and saves it at once.
#[derive(Debug, PartialEq)]
pub struct HelpMenu {
    pub tab: HelpTab,
    /// The first line shown on the keys tab. Drawing keeps it in range.
    pub scroll: Cell<usize>,
    /// The row under the cursor on the settings tab: a setting, or a theme
    /// from `THEMES` on, in `theme::names()`.
    pub selected: usize,
    /// Why the last setting picked couldn't be saved.
    pub error: Option<String>,
}

impl HelpMenu {
    /// The settings tab's rows, as tuitube's: pausing when the headphones
    /// are unplugged, the animations, then the themes.
    pub const PAUSE_ON_UNPLUG: usize = 0;
    pub const ANIMATIONS: usize = 1;
    pub const THEMES: usize = 2;
}

/// The command line while it's open (`/`): what's typed, the row under the
/// cursor in what it matches (`explore::matching`), and the first row its
/// box shows (drawing keeps the cursor in view).
#[derive(Debug, Default, PartialEq)]
pub struct Search {
    pub typed: Typed,
    pub selected: usize,
    pub scroll: Cell<usize>,
}
impl Search {
    /// The cursor on the best match for what's typed that isn't the feed
    /// `shown`, as it changes (typed or pasted).
    fn reselect(&mut self, shown: Option<Category>) {
        let offered = explore::matching(self.typed.text());
        self.selected = offered.iter().position(|c| *c != shown).unwrap_or(0);
    }
}

/// The command line `:` opens, as tuitube's: what's typed, and why the line
/// Enter ran couldn't run (until it's edited).
#[derive(Debug, Default, PartialEq)]
pub struct CommandLine {
    pub typed: Typed,
    pub error: Option<String>,
}

/// A command `:` runs: its name, its argument as its box shows it, and what
/// it does (tuitube's `commands::Spec`, cut down).
#[derive(Debug, PartialEq)]
pub struct Spec {
    pub name: &'static str,
    pub hint: &'static str,
    pub about: &'static str,
}

pub const COMMANDS: [Spec; 1] = [Spec {
    name: "open",
    hint: "<link>",
    about: "play a TikTok link",
}];

/// The commands `:`'s box offers for `typed`: those whose names start with
/// it, while it's one word.
pub fn completions(typed: &str) -> Vec<&'static Spec> {
    let typed = typed.trim_start();
    if typed.contains(' ') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(typed))
        .collect()
}

/// A link being opened (`:open`, pasted, or `tuitok LINK`), or why the last
/// one couldn't be: said on the status line, a failure until the next key.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkState {
    Opening,
    Failed(String),
}

pub struct App {
    pub colors: Colors,
    /// The theme in use (its id in `theme::BUILT_IN`), the data folder
    /// it's saved in when picked (none in the demo and tests), and the `?`
    /// popup while it's open.
    pub theme: &'static str,
    settings_in: Option<PathBuf>,
    pub help: Option<HelpMenu>,
    /// tuitok runs in Pepper (`pepper::port`), and that Pepper takes holes
    /// in the picture's box (`pepper::takes_holes`), so it plays on around a
    /// popup; set by `main`.
    pub pepper: bool,
    pub pepper_holes: bool,
    /// What the last frame drew over the rest (the `?` popup, the boxes
    /// over the bottom line), for the holes in Pepper's picture. Set by
    /// `ui::draw`.
    pub drawn_over: RefCell<Vec<Rect>>,
    /// A cell's size in pixels, and the terminal's size in cells: the box's
    /// size in pixels comes from both.
    pub cell: (u16, u16),
    pub size: (u16, u16),
    /// What the feed is: For You (`None`), or one of Explore's categories,
    /// picked on the command line `/` opens; and that while it's open.
    pub category: Option<Category>,
    pub search: Option<Search>,
    /// The command line `:` opens, while it's open; the link being opened
    /// (or why the last couldn't be), how many have been (each one's video
    /// comes with its number), and the one `tuitok LINK` opens first.
    pub command: Option<CommandLine>,
    pub link: Option<LinkState>,
    links_opened: u64,
    first_link: Option<Link>,
    /// The guest Explore's pages are fetched as, once there is one (only in
    /// memory), and the category whose page is being fetched.
    explore_guest: Option<explore::Guest>,
    exploring: Option<Category>,
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
    /// What was watched before the feed shown (what the last run's guest
    /// watched, `guest.rs`, then this run's feeds before), and the furthest
    /// video shown in this one: handed to the next run.
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
    /// Downloads under way, by video, and the last one's number (each
    /// download's file name and events carry its own).
    underway: HashMap<VideoId, Underway>,
    downloads_started: u64,
    /// The video shown while it waits to stream ([`App::wait_to_stream`]).
    waiting: Option<VideoId>,
    /// How many times each video's download failed: it's tried again,
    /// `cache::TRIES` in all.
    failures: HashMap<VideoId, u32>,
    /// How loud, in percent (`-` and `+`), what settings.toml has (it's
    /// written when tuitok ends if that changed), and the sound off (`m`,
    /// for this run).
    pub volume: u32,
    volume_kept: u32,
    pub muted: bool,
    /// Pause when the headphones are unplugged (`?`'s settings), whether
    /// their going is watched, and the device that went since you last
    /// played something: what loads meanwhile starts paused.
    pub pause_on_unplug: bool,
    pub output_watch: OutputWatch,
    pub held: Option<String>,
    /// The animations (`?`'s settings): the screen fading in, the panel
    /// sweeping in, the popup opening out; what moves, and what the frame
    /// before showed (`effects`). They run only once `main` or `--demo`
    /// sets `live`.
    pub animations: bool,
    pub effects: RefCell<crate::effects::Effects>,
    /// The splash screen, while it's up over the first frames (`splash`,
    /// `keep_splash`).
    pub splash: Option<crate::splash::Splash>,
    /// The program to play with and how (`main` decides: pepper-play in
    /// Pepper, else mpv's window), the player once started, how many have
    /// been started (each one's events carry its number), what it was told
    /// to play, that load's number and the playlist entry the player made
    /// of it (what it says of any other is dropped), and whether it ended
    /// by itself (its window closed): then none starts again until a key
    /// asks.
    player_with: Option<(PathBuf, Kind)>,
    player: Option<Player>,
    players: u64,
    loaded: Option<VideoId>,
    load: u64,
    entry: Option<i64>,
    pub player_gone: bool,
    /// Pepper's token for the player's picture (pepper-play only), and what
    /// Pepper was last told.
    token: Option<String>,
    placed: Option<Placed>,
    /// Covers by video, how the terminal draws pictures (none in tests),
    /// and the client for TikTok's image hosts.
    pub covers: HashMap<VideoId, Cover>,
    picker: Option<Picker>,
    cover_client: Option<reqwest::Client>,
    /// Top comments by video, and the client for `www.tiktok.com`: its
    /// comment list, Explore's pages and a video's page; and the one that
    /// asks a short link where it leads, following no redirect.
    pub comments: HashMap<VideoId, Comments>,
    www_client: Option<reqwest::Client>,
    short_client: Option<reqwest::Client>,
    /// `f` in Pepper: the video fills the tab; and, where Pepper takes
    /// holes, when what plays stops showing over it (`shows_controls`): for
    /// `CONTROLS_FOR` after anything's done.
    pub fullscreen: bool,
    pub controls_until: Option<Instant>,
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
        let feed = first_state(&engines);
        Self {
            colors: Colors::named(theme::DEFAULT),
            theme: theme::DEFAULT,
            settings_in: None,
            help: None,
            pepper,
            pepper_holes: false,
            drawn_over: RefCell::new(Vec::new()),
            cell: cell_size().unwrap_or(GUESSED_CELL),
            size: crossterm::terminal::size().unwrap_or(GUESSED_SIZE),
            category: None,
            search: None,
            command: None,
            link: None,
            links_opened: 0,
            first_link: None,
            explore_guest: None,
            exploring: None,
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
            underway: HashMap::new(),
            downloads_started: 0,
            waiting: None,
            failures: HashMap::new(),
            volume: 100,
            volume_kept: 100,
            muted: false,
            pause_on_unplug: true,
            output_watch: OutputWatch::Never,
            held: None,
            animations: true,
            effects: RefCell::default(),
            splash: None,
            player_with,
            player: None,
            players: 0,
            loaded: None,
            load: 0,
            entry: None,
            player_gone: false,
            token: None,
            placed: None,
            covers: HashMap::new(),
            picker: None,
            cover_client: None,
            comments: HashMap::new(),
            www_client: None,
            short_client: None,
            fullscreen: false,
            controls_until: None,
            demo: false,
            behind_multiplexer: None,
            client: None,
            tx: None,
            quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        // The terminal window closed (SIGHUP), or tuitok was asked to end
        // (SIGTERM, also sent by a task's panic: `screen::set_panic_hook`):
        // it quits as with q, so the player quits, Pepper's box goes and the
        // session's folder is deleted. Taken before any task starts.
        let mut hangup = signal(SignalKind::hangup())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let (tx, mut app_events) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        if !self.demo {
            self.client = Some(cache::client(crate::video::video_url_allowed));
            self.cover_client = Some(cache::client(crate::video::image_url_allowed));
            self.www_client = Some(cache::client(crate::video::www_url_allowed));
            self.short_client = Some(cache::client(|_| false));
        }
        self.want_covers();
        self.want_comments();
        // `tuitok LINK`: its video first, then the feed (`on_opened`), so
        // the feed's first doesn't play before it.
        match self.first_link.take() {
            Some(link) => self.open_link(link),
            None => self.fetch_more(),
        }
        let mut terminal_events = EventStream::new();
        let mut result = Ok(());
        while !self.quit {
            self.advance_splash(Instant::now());
            // A frame that can't be drawn ends tuitok as a quit does; when
            // the tab was closed (the hangup not read yet), it is one.
            let drawn = terminal
                .draw(|frame| ui::draw(frame, self))
                .and_then(|_| write_to_terminal(&self.placement()));
            if let Err(e) = drawn {
                if !crate::screen::gone() {
                    result = Err(e);
                }
                break;
            }
            // The next frame while something moves (the animations, the
            // splash screen), or the controls over Pepper's full screen go.
            let hide_in = self.controls_hide_in(Instant::now());
            let wake = [hide_in, self.frame_in()].into_iter().flatten().min();
            tokio::select! {
                event = terminal_events.next() => match event {
                    Some(Ok(event)) => self.on_event(event),
                    Some(Err(e)) => {
                        if !crate::screen::gone() {
                            result = Err(e);
                        }
                        break;
                    }
                    None => break,
                },
                Some(event) = app_events.recv() => self.on_app_event(event),
                _ = hangup.recv() => self.quit = true,
                _ = terminate.recv() => self.quit = true,
                _ = tokio::time::sleep(wake.unwrap_or_default()), if wake.is_some() => {}
            }
        }
        // A tab that's gone has no box to take away; the player quits anyway.
        if let Some(placed) = self.placed.take() {
            let _ = write_to_terminal(&pepper::remove(&placed.token));
        }
        if let Some(player) = self.player.take() {
            player.quit().await;
        }
        result
    }

    /// Keeps the splash screen drawn since `since` up over the first frames
    /// while what they show comes (`splash`).
    pub fn keep_splash(&mut self, since: Instant) {
        self.splash = Some(crate::splash::Splash::new(since));
    }

    /// The splash screen moves on, and goes once it's been up long enough
    /// and the first screen is there, or it's been up its most.
    fn advance_splash(&mut self, now: Instant) {
        let ready = self.first_screen_ready();
        if let Some(splash) = &mut self.splash
            && !splash.advance(now, ready)
        {
            self.splash = None;
        }
    }

    /// Whether the first screen is all there for the splash to go: the
    /// video shown, and its cover (or none to wait for).
    fn first_screen_ready(&self) -> bool {
        self.current()
            .is_some_and(|video| match self.covers.get(&video.id) {
                Some(Cover::Ready { encoded, .. }) => encoded.is_some(),
                Some(Cover::Failed) => true,
                Some(Cover::Loading) => false,
                None => video.cover.is_none(),
            })
    }

    /// How long until the next frame while something moves: the
    /// animations, or the splash screen.
    fn frame_in(&self) -> Option<Duration> {
        let effects = self.effects.borrow();
        let moving = effects.moving().then(|| effects.every());
        let splash = self.splash.as_ref().map(crate::splash::Splash::every);
        [moving, splash].into_iter().flatten().min()
    }

    /// The video shown, if there is one.
    pub fn current(&self) -> Option<&Video> {
        self.videos.get(self.at)
    }

    /// What the feed shown is called: For You, or its category's name.
    pub fn feed_name(&self) -> &'static str {
        explore::name(self.category)
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

    /// Whether the terminal draws the covers as pictures (kitty's, sixel,
    /// iTerm2's) rather than in colored half blocks: the animations leave
    /// their cells be.
    pub fn draws_pictures(&self) -> bool {
        self.picker
            .as_ref()
            .is_some_and(|picker| picker.protocol_type() != ProtocolType::Halfblocks)
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

    /// `tuitok LINK`: its video plays first, then the feed.
    pub fn open_first(&mut self, link: Link) {
        self.first_link = Some(link);
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
        self.pause_on_unplug = settings.pause_on_unplug;
        self.animations = settings.animations;
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

    /// Whether that picture is in Pepper's box (not in mpv's window), and
    /// the splash screen isn't up over it. The `?` popup, or a box over the
    /// bottom line where it's over the video, would be under it: where
    /// Pepper takes holes they're cut in it for them, else it's taken away
    /// while one is open (as in tuitube).
    pub fn picture_in_box(&self) -> bool {
        self.shows_picture()
            && self.token.is_some()
            && self.splash.is_none()
            && (self.pepper_holes || (self.help.is_none() && !self.offers_cover_video()))
    }

    /// Whether a box over the bottom line (`/`'s categories, `:`'s commands)
    /// is open over the video's side: it goes over the panel, so the video
    /// plays on beside it, but where there's no panel (a narrow window, or
    /// `f`) it's over the video.
    pub fn offers_cover_video(&self) -> bool {
        (self.search.is_some() || !self.command_offers().is_empty()) && self.areas().panel.is_none()
    }

    /// The commands `:`'s box offers, while it's open.
    pub fn command_offers(&self) -> Vec<&'static Spec> {
        self.command
            .as_ref()
            .map_or_else(Vec::new, |line| completions(line.typed.text()))
    }

    /// Whether the player was told to play the video shown and has no
    /// frame of it yet.
    pub fn loading(&self) -> bool {
        self.loads_shown() && !self.shows_picture()
    }

    /// Whether the player was told to play the video shown.
    fn loads_shown(&self) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|id| Some(id) == self.current().map(|v| &v.id))
    }

    /// Starts fetching more of the feed shown, unless that's under way.
    fn fetch_more(&mut self) {
        match self.category {
            Some(category) => self.explore(category),
            None => self.fetch_for_you(),
        }
    }

    /// Starts fetching a page of `category`, unless one is being fetched.
    fn explore(&mut self, category: Category) {
        if self.exploring == Some(category) {
            return;
        }
        let (Some(client), Some(tx)) = (self.www_client.clone(), self.tx.clone()) else {
            return;
        };
        self.exploring = Some(category);
        self.feed = FeedState::Exploring;
        let guest = self.explore_guest.clone();
        tokio::spawn(async move {
            let page = explore::fetch(&client, category, guest)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Explored { category, page });
        });
    }

    /// Starts fetching For You's next batch, unless one is being fetched.
    fn fetch_for_you(&mut self) {
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
                    seen: Arc::new(self.seen.clone()),
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
            // For You's videos, while it's shown.
            AppEvent::Found(videos) if self.category.is_none() => {
                self.add(videos);
                self.prefetch();
                self.want_covers();
                self.want_comments();
                self.play_current();
            }
            AppEvent::Found(_) => {}
            AppEvent::Batch(batch) if self.category.is_none() => {
                self.on_batch(batch);
                self.prefetch();
                self.want_covers();
                self.want_comments();
                self.play_current();
            }
            AppEvent::Batch(batch) => self.put_batch_aside(batch),
            AppEvent::Explored { category, page } => {
                self.on_explored(category, page);
                self.prefetch();
                self.want_covers();
                self.want_comments();
                self.play_current();
            }
            AppEvent::Opened { number, video } if number == self.links_opened => {
                self.on_opened(video);
            }
            AppEvent::Opened { .. } => {}
            AppEvent::Rested => self.refill(),
            // What comes for a video no longer waited for (of a feed no
            // longer shown) is dropped.
            AppEvent::CommentsFetched { id, .. }
                if !matches!(self.comments.get(&id), Some(Comments::Loading)) => {}
            AppEvent::CoverFetched { id, .. }
                if !matches!(self.covers.get(&id), Some(Cover::Loading)) => {}
            AppEvent::CommentsFetched { id, comments } => {
                let comments = comments.map_or(Comments::Failed, Comments::Ready);
                self.comments.insert(id, comments);
            }
            AppEvent::Progress {
                id,
                number,
                got,
                of,
            } if self.is_underway(&id, number) => {
                if let Some(Download::Getting {
                    got: was,
                    of: total,
                }) = self.downloads.get_mut(&id)
                {
                    *was = got;
                    *total = of.or(*total);
                }
            }
            AppEvent::Progress { .. } => {}
            // A download stopped (or of a feed no longer shown) that ended
            // just before: its file goes.
            AppEvent::Downloaded {
                id,
                number,
                outcome,
            } if !self.is_underway(&id, number) => {
                if let Download::Ready(path) = outcome {
                    let _ = std::fs::remove_file(path);
                }
            }
            AppEvent::Downloaded { id, outcome, .. } => {
                self.underway.remove(&id);
                if matches!(outcome, Download::Failed(_)) {
                    self.try_again_later(&id);
                }
                self.downloads.insert(id, outcome);
                self.prefetch();
                self.play_current();
            }
            AppEvent::TryAgain(id) => self.try_again(&id),
            AppEvent::Waited(id) => self.stream_instead(&id),
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
            AppEvent::OutputGone(name) => self.on_output_gone(name),
            AppEvent::OutputUnwatched(why) => {
                self.output_watch = OutputWatch::Failed(text::one_line(&why, 200))
            }
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
                self.rest();
            }
        }
        self.batch_from = self.videos.len();
        if ok && self.ramping() {
            self.fetch_more();
        }
    }

    /// For You's batch ended while a category is shown: the browser is free,
    /// and the guest goes on; the videos are dropped.
    fn put_batch_aside(&mut self, batch: Result<Batch, String>) {
        self.fetching = false;
        if let Ok(batch) = batch
            && !batch.cookies.is_empty()
        {
            self.cookies = batch.cookies;
        }
    }

    /// A page of `category` came, or didn't. Its guest goes on to the next
    /// page; its videos are added if `category` is the one shown.
    fn on_explored(&mut self, category: Category, page: Result<explore::Page, String>) {
        if self.exploring == Some(category) {
            self.exploring = None;
        }
        let page = match page {
            Ok(page) => {
                self.explore_guest = Some(page.guest.clone());
                Ok(page)
            }
            Err(e) => {
                // It was a new guest's try too (`explore::fetch`).
                self.explore_guest = None;
                Err(e)
            }
        };
        if self.category != Some(category) {
            return;
        }
        match page {
            Ok(page) => {
                let before = self.videos.len();
                self.add(page.videos);
                let new = self.videos.len() - before;
                self.feed = FeedState::Explored {
                    new,
                    took: page.took,
                };
                // All seen before: the next page waits, as after a failure.
                if new == 0 {
                    self.rest();
                } else {
                    self.failed_at = None;
                }
            }
            Err(e) => {
                self.feed = FeedState::Failed(text::one_line(&e, 300));
                self.rest();
            }
        }
    }

    /// Opens `link`: its video is fetched (no browser: `links::fetch`), then
    /// shown next ([`App::on_opened`]). A link opened meanwhile takes its
    /// place.
    fn open_link(&mut self, link: Link) {
        if self.demo {
            self.link = Some(LinkState::Failed("The demo opens no links".into()));
            return;
        }
        self.links_opened += 1;
        self.link = Some(LinkState::Opening);
        let (Some(client), Some(short_client), Some(tx)) = (
            self.www_client.clone(),
            self.short_client.clone(),
            self.tx.clone(),
        ) else {
            return;
        };
        let number = self.links_opened;
        tokio::spawn(async move {
            let video = links::fetch(&client, &short_client, link)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Opened { number, video });
        });
    }

    /// The video a link is to came: it's shown next (`go` fetches more of
    /// the feed if it wants it). Or it didn't, and the status line says
    /// why; then the feed is fetched if it wants more (after `tuitok LINK`,
    /// it waited for this).
    fn on_opened(&mut self, video: Result<Video, String>) {
        match video {
            Ok(video) => {
                self.link = None;
                self.show_next(video);
            }
            Err(e) => {
                let why = text::one_line(&e, 300);
                self.link = Some(LinkState::Failed(format!("The link failed: {why}")));
                self.refill();
            }
        }
    }

    /// Shows `video` where `j` goes from the one shown, so `k` goes back to
    /// it and `j` goes on with the feed. One the feed has already is gone
    /// back to if it's behind, else moved there, so the videos between it
    /// and the one shown aren't skipped (and counted as watched). It's shown
    /// even if it was watched before (it was asked for), and the feed
    /// doesn't send it again.
    fn show_next(&mut self, video: Video) {
        match self.videos.iter().position(|v| v.id == video.id) {
            Some(at) if at <= self.at => {
                self.go(at as isize - self.at as isize);
                return;
            }
            Some(at) => {
                self.videos.remove(at);
                if at < self.batch_from {
                    self.batch_from -= 1;
                }
                if at <= self.furthest {
                    self.furthest -= 1;
                }
            }
            None => {
                self.seen.insert(video.id.clone());
                // Watched before (in a run or a feed before): watched again
                // now, and kept once (`carry_out`).
                self.watched_before.retain(|id| *id != video.id);
            }
        }
        let to = if self.videos.is_empty() {
            0
        } else {
            self.at + 1
        };
        // The furthest shown stays the same video.
        if !self.videos.is_empty() && to <= self.furthest {
            self.furthest += 1;
        }
        self.videos.insert(to, video);
        // Not the batch's: its count (`found_so_far`) leaves it out.
        self.batch_from += 1;
        self.go(to as isize - self.at as isize);
    }

    /// A fetch failed, or brought nothing new: the next waits `RETRY_AFTER`,
    /// then starts on its own if the feed shown still wants more
    /// ([`App::wants_more`]; `r` starts it at once).
    fn rest(&mut self) {
        self.failed_at = Some(Instant::now());
        let Some(tx) = self.tx.clone() else {
            return;
        };
        tokio::spawn(async move {
            tokio::time::sleep(RETRY_AFTER).await;
            let _ = tx.send(AppEvent::Rested);
        });
    }

    /// Shows `category` (For You: `None`) in place of the feed shown, from
    /// its start: the videos of the one before go, with their downloads,
    /// covers and comments, and what of them was shown counts as watched.
    /// In the demo only the name changes.
    fn show(&mut self, category: Option<Category>) {
        if category == self.category {
            return;
        }
        self.category = category;
        self.held = None;
        if self.demo {
            return;
        }
        let shown = self.videos.len().min(self.furthest + 1);
        let watched = self.videos[..shown].iter().map(|v| v.id.clone());
        self.watched_before.extend(watched);
        self.stop_playing();
        self.player_gone = false;
        // Downloads under way stop, their part files with them
        // (`cache::download`).
        for (_, underway) in self.underway.drain() {
            underway.abort.abort();
        }
        for (_, download) in self.downloads.drain() {
            if let Download::Ready(path) = download {
                let _ = std::fs::remove_file(path);
            }
        }
        self.waiting = None;
        self.failures.clear();
        self.covers.clear();
        self.comments.clear();
        self.videos.clear();
        self.spare.clear();
        (self.at, self.furthest) = (0, 0);
        (self.batches, self.batch_from) = (0, 0);
        self.batch_size = feed::batch_size(0);
        self.play_error = None;
        self.failed_at = None;
        self.feed = match category {
            Some(_) => FeedState::Exploring,
            None => first_state(&self.engines),
        };
        self.fetch_more();
    }

    fn on_player(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Loaded { load, entry } => {
                if self.loaded.is_some() && load == self.load {
                    self.entry = Some(entry);
                }
            }
            PlayerEvent::Restarted(entry) if self.entry == Some(entry) => {
                // A new video's first frame: what plays shows over it a while.
                let new = self.playing != self.loaded;
                self.playing = self.loaded.clone();
                if new {
                    self.show_controls();
                }
            }
            PlayerEvent::Position(entry, seconds) if self.entry == Some(entry) => {
                self.position = seconds;
            }
            PlayerEvent::Duration(entry, seconds) if self.entry == Some(entry) => {
                self.duration = Some(seconds);
            }
            PlayerEvent::Failed(entry, e) if self.entry == Some(entry) => {
                self.playing = None;
                self.play_error = Some(format!("It can't play: {e}"));
            }
            // Of a file before the one loaded: it looped, or ended, just as
            // the next was loaded.
            PlayerEvent::Restarted(_)
            | PlayerEvent::Position(..)
            | PlayerEvent::Duration(..)
            | PlayerEvent::Failed(..) => {}
            // From paused to playing (space, or mpv's window): you played
            // it, and nothing waits for the headphones any more.
            PlayerEvent::Paused(paused) => {
                if self.paused && !paused {
                    self.held = None;
                }
                if self.paused != paused {
                    self.show_controls();
                }
                self.paused = paused;
            }
            // Changed in mpv's window too: the next video plays as loud.
            PlayerEvent::Volume(volume) => self.volume = volume.round().clamp(0.0, 100.0) as u32,
            PlayerEvent::Muted(muted) => self.muted = muted,
            PlayerEvent::Exited => {
                self.player = None;
                self.token = None;
                self.loaded = None;
                self.entry = None;
                self.playing = None;
                // Its window closed: a download ending isn't a reason to
                // open another.
                self.player_gone = true;
            }
        }
    }

    /// What would play for the video shown, if it can play yet: its file
    /// once downloaded, else its address streamed (too big to download, or
    /// the download failed, or took too long: [`App::stream_instead`]).
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
    /// yet, the player stops, so the last one doesn't play on under it, and
    /// the video waits to stream instead.
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
            // Why the one before couldn't play isn't this one's.
            self.play_error = None;
            self.wait_to_stream(&id);
            return;
        };
        if !self.start_player() {
            return;
        }
        // Paused if the headphones went since you last played something.
        let paused = self.held.is_some();
        if let Some(player) = &self.player {
            self.load = player.load(&target, paused);
        }
        self.entry = None;
        self.loaded = Some(id);
        self.playing = None;
        self.position = 0.0;
        self.duration = None;
        self.paused = paused;
        self.play_error = None;
        // From the first video played: what an unplug could pause.
        self.watch_output();
    }

    /// Starts watching the sound's device (`crate::output`) the first time
    /// something plays with `pause_on_unplug` on, for the rest of the run.
    fn watch_output(&mut self) {
        if self.output_watch == OutputWatch::Later
            && self.pause_on_unplug
            && let Some(tx) = self.tx.clone()
        {
            self.output_watch = OutputWatch::Watching;
            crate::output::watch(tx);
        }
    }

    /// The device playing the sound went (`name`): headphones unplugged, or
    /// a Bluetooth one turned off. With `pause_on_unplug`, what plays
    /// pauses, as on an iPhone, rather than go on from the speakers, and
    /// what loads before you play something starts paused (`held`): also
    /// with nothing shown yet, or the video shown paused already, as
    /// tuitube's.
    fn on_output_gone(&mut self, name: String) {
        if !self.pause_on_unplug {
            return;
        }
        // Sent though mpv may have said it's paused: it may be about to
        // answer a space that plays it. Paused twice, it stays paused.
        if self.loads_shown()
            && let Some(player) = &self.player
        {
            player.pause();
        }
        self.held = Some(name);
    }

    fn stop_playing(&mut self) {
        if self.loaded.take().is_some()
            && let Some(player) = &self.player
        {
            player.stop();
        }
        self.entry = None;
        self.playing = None;
    }

    /// The video shown can't play yet: unless it can within `STREAM_AFTER`,
    /// it streams then ([`App::stream_instead`]).
    fn wait_to_stream(&mut self, id: &VideoId) {
        if self.waiting.as_ref() == Some(id) {
            return;
        }
        let Some(tx) = self.tx.clone() else {
            return;
        };
        self.waiting = Some(id.clone());
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(STREAM_AFTER).await;
            let _ = tx.send(AppEvent::Waited(id));
        });
    }

    /// The video shown waited `STREAM_AFTER` for its download (under way,
    /// or waiting for its turn): if it still waits, it streams, and its
    /// download stops, as one that plays streamed isn't downloaded as well.
    fn stream_instead(&mut self, id: &VideoId) {
        if self.waiting.as_ref() == Some(id) {
            self.waiting = None;
        }
        let still_waits = self.current().is_some_and(|v| &v.id == id)
            && matches!(
                self.downloads.get(id),
                None | Some(Download::Getting { .. })
            );
        if !still_waits {
            return;
        }
        self.cancel(id);
        self.downloads.insert(id.clone(), Download::Streams);
        self.prefetch();
        self.play_current();
    }

    /// Starts the player unless it runs (or ended by itself, until a key
    /// asks); whether it runs.
    fn start_player(&mut self) -> bool {
        if self.player.is_some() {
            return true;
        }
        if self.player_gone {
            return false;
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
    /// while the video shown plays, with holes where the frame drew over it
    /// if Pepper takes them (placed again when the box or its holes
    /// change), taken away otherwise. Empty when nothing changed, and always
    /// outside Pepper (no token).
    fn placement(&mut self) -> String {
        let want = self
            .token
            .clone()
            .filter(|_| self.picture_in_box())
            .map(|token| {
                let area = self.video_area();
                let holes = if self.pepper_holes {
                    pepper::holes(area, &self.drawn_over.borrow())
                } else {
                    Vec::new()
                };
                Placed { token, area, holes }
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
            out.push_str(&pepper::place(&want.token, want.area, &want.holes));
        }
        self.placed = want;
        out
    }

    /// What to do so the video shown and the `cache::AHEAD` after it are
    /// downloaded, nearest first, `cache::PARALLEL` at a time: start a
    /// download (numbered after the last), or mark a video that won't be
    /// downloaded (too big, or nothing in it plays), which takes no turn.
    fn plan(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut running = self.underway.len();
        let mut number = self.downloads_started;
        let box_pixels = self.box_pixels();
        let end = (self.at + cache::AHEAD + 1).min(self.videos.len());
        for video in self.videos.get(self.at..end).unwrap_or_default() {
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
                // Its turn comes when one under way ends.
                Some(_) if running >= cache::PARALLEL => {}
                Some(r) => {
                    running += 1;
                    number += 1;
                    steps.push(Step::Start(
                        number,
                        cache::Job {
                            path: self.cache.join(cache::file_name(&id, number, r)),
                            id,
                            rendition: r.clone(),
                            token: video.token.clone(),
                        },
                    ));
                }
            }
        }
        steps
    }

    /// Stops the downloads no longer wanted (of videos behind the one
    /// shown, or more than `cache::AHEAD` after it), then carries out
    /// [`App::plan`].
    fn prefetch(&mut self) {
        self.cancel_unwanted();
        let (Some(client), Some(tx)) = (self.client.clone(), self.tx.clone()) else {
            return;
        };
        for step in self.plan() {
            match step {
                Step::Mark(id, outcome) => {
                    self.downloads.insert(id, outcome);
                }
                Step::Start(number, job) => {
                    self.downloads_started = number;
                    let getting = Download::Getting {
                        got: 0,
                        of: job.rendition.bytes,
                    };
                    let id = job.id.clone();
                    self.downloads.insert(id.clone(), getting);
                    let (client, tx) = (client.clone(), tx.clone());
                    let task = tokio::spawn(async move {
                        let progress = {
                            let (tx, id) = (tx.clone(), job.id.clone());
                            move |got, of| {
                                let _ = tx.send(AppEvent::Progress {
                                    id: id.clone(),
                                    number,
                                    got,
                                    of,
                                });
                            }
                        };
                        let outcome = cache::download(&client, &job, progress).await;
                        let _ = tx.send(AppEvent::Downloaded {
                            id: job.id,
                            number,
                            outcome,
                        });
                    });
                    let abort = task.abort_handle();
                    self.underway.insert(id, Underway { number, abort });
                }
            }
        }
    }

    /// Whether download `number` of video `id` is still wanted: not
    /// stopped, nor of a feed no longer shown.
    fn is_underway(&self, id: &VideoId, number: u64) -> bool {
        self.underway.get(id).is_some_and(|u| u.number == number)
    }

    /// Stops the downloads of videos not among the video shown and the
    /// `cache::AHEAD` after it, so those get their turn.
    fn cancel_unwanted(&mut self) {
        let end = (self.at + cache::AHEAD + 1).min(self.videos.len());
        let wanted = self.videos.get(self.at..end).unwrap_or_default();
        let unwanted: Vec<VideoId> = self
            .underway
            .keys()
            .filter(|id| !wanted.iter().any(|v| &v.id == *id))
            .cloned()
            .collect();
        for id in unwanted {
            self.cancel(&id);
        }
    }

    /// Stops video `id`'s download, if one's under way; its part file goes
    /// with it (`cache::download`).
    fn cancel(&mut self, id: &VideoId) {
        if let Some(underway) = self.underway.remove(id) {
            underway.abort.abort();
            self.downloads.remove(id);
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
        let (Some(client), Some(tx)) = (self.www_client.clone(), self.tx.clone()) else {
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
        // Under the splash screen nothing's seen: a key or a paste takes it
        // away and does nothing else, but quit (q, Ctrl-C). Once it fades,
        // keys work.
        if let Some(splash) = &mut self.splash
            && matches!(
                &event,
                Event::Key(KeyEvent {
                    kind: KeyEventKind::Press | KeyEventKind::Repeat,
                    ..
                }) | Event::Paste(_)
            )
            && splash.leave(Instant::now())
        {
            if let Event::Key(key) = &event
                && (key.code == KeyCode::Char('q') || ctrl_c(key))
            {
                self.quit = true;
            }
            return;
        }
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.on_key(key);
                self.show_controls();
            }
            Event::Resize(columns, rows) => {
                // A terminal that doesn't say keeps the size found before
                // (by the picker, `set_picker`).
                self.cell = cell_size().unwrap_or(self.cell);
                self.size = (columns, rows);
                self.want_covers();
            }
            // A paste, whole (`main` asks the terminal for bracketed ones).
            Event::Paste(pasted) => self.on_paste(&pasted),
            _ => {}
        }
    }

    /// A paste, as tuitube takes one: into the command line open (`:` or
    /// `/`), as one line; with neither, a TikTok link opens. Not while the
    /// `?` popup is open.
    fn on_paste(&mut self, pasted: &str) {
        let shown = self.category;
        if self.help.is_some() {
            return;
        }
        if let Some(line) = self.command.as_mut() {
            paste_into(&mut line.typed, pasted, MAX_COMMAND);
            line.error = None;
        } else if let Some(search) = self.search.as_mut() {
            paste_into(&mut search.typed, pasted, MAX_SEARCH);
            search.reselect(shown);
        } else {
            match links::parse(text::first_chars(pasted, links::MAX_LINK + 1)) {
                Some(link) => self.open_link(link),
                // Not over a link still being opened.
                None if self.link != Some(LinkState::Opening) => {
                    let why = "What was pasted isn't a TikTok video's link";
                    self.link = Some(LinkState::Failed(why.into()));
                }
                None => {}
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if ctrl_c(&key) {
            self.quit = true;
            return;
        }
        // Why a link failed is said until the next key, typed in a line too.
        if matches!(self.link, Some(LinkState::Failed(_))) {
            self.link = None;
        }
        if self.help.is_some() {
            self.on_help_key(key);
            return;
        }
        if self.search.is_some() {
            self.on_search_key(key);
            return;
        }
        if self.command.is_some() {
            self.on_command_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.go(1),
            KeyCode::Char('k') | KeyCode::Up => self.go(-1),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') => self.set_muted(!self.muted),
            KeyCode::Char('-') => self.set_volume(self.volume.saturating_sub(VOLUME_STEP)),
            KeyCode::Char('+' | '=') => self.set_volume(self.volume + VOLUME_STEP),
            KeyCode::Char('h' | ',') | KeyCode::Left => self.with_player(|p| p.seek(-SEEK)),
            KeyCode::Char('l' | '.') | KeyCode::Right => self.with_player(|p| p.seek(SEEK)),
            KeyCode::Char('f') => self.toggle_fullscreen(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('/') => self.open_search(),
            KeyCode::Char(':') => self.command = Some(CommandLine::default()),
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
        let theme = theme::names()
            .iter()
            .position(|(id, _)| *id == self.theme)
            .unwrap_or(0);
        self.help = Some(HelpMenu {
            tab: HelpTab::Keys,
            scroll: Cell::new(0),
            selected: HelpMenu::THEMES + theme,
            error: None,
        });
    }

    /// Keys while the `?` popup is open, as tuitube's: Tab (or `h` `l`, ←
    /// →) the other tab, `j` `k` scroll the keys or move the settings'
    /// cursor, Enter or Space turn the setting under it on or off or pick
    /// the theme under it, Esc, `q` or `?` close.
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
                let saved = match menu.selected {
                    HelpMenu::PAUSE_ON_UNPLUG => self.toggle_pause_on_unplug(),
                    HelpMenu::ANIMATIONS => self.toggle_animations(),
                    row => self.pick_theme(row - HelpMenu::THEMES),
                };
                if let Some(menu) = &mut self.help {
                    menu.error = saved.err();
                }
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
                let last = HelpMenu::THEMES + theme::names().len() - 1;
                menu.selected = menu.selected.saturating_add_signed(delta).min(last);
            }
        }
    }

    /// `/`: the command line opens, offering For You and every category, the
    /// cursor on the one shown.
    fn open_search(&mut self) {
        let selected = explore::matching("")
            .iter()
            .position(|c| *c == self.category)
            .unwrap_or(0);
        self.search = Some(Search {
            selected,
            ..Search::default()
        });
    }

    /// Keys while the command line is open: what's typed narrows what it
    /// offers, with the cursor on the best match that isn't the feed shown;
    /// ↓ ↑ (or Tab, Ctrl-n Ctrl-p) move it, Enter shows what's under it,
    /// Esc closes it; the rest edit the line at its cursor (`Typed::edit`:
    /// ← → and Home End move it, Backspace, Ctrl-w and Ctrl-u rub out as in
    /// a shell).
    fn on_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shown = self.category;
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let offered = explore::matching(search.typed.text());
        let last = offered.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => {
                if let Some(&category) = offered.get(search.selected) {
                    self.search = None;
                    self.show(category);
                }
            }
            KeyCode::Down | KeyCode::Tab => search.selected = (search.selected + 1).min(last),
            KeyCode::Char('n') if ctrl => search.selected = (search.selected + 1).min(last),
            KeyCode::Up | KeyCode::BackTab => search.selected = search.selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => search.selected = search.selected.saturating_sub(1),
            _ => {
                if search.typed.edit(&key, MAX_SEARCH) {
                    search.reselect(shown);
                }
            }
        }
    }

    /// Keys while the command line `:` is open, as tuitube's: Tab completes
    /// the command its box offers, Enter runs the line, Esc closes it;
    /// typing and rubbing out as on `/`'s.
    fn on_command_key(&mut self, key: KeyEvent) {
        let Some(line) = self.command.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.command = None,
            KeyCode::Enter => self.run_command(),
            KeyCode::Tab => {
                if let Some(spec) = completions(line.typed.text()).first() {
                    line.typed = Typed::new(format!("{} ", spec.name));
                    line.error = None;
                }
            }
            _ => {
                if line.typed.edit(&key, MAX_COMMAND) {
                    line.error = None;
                }
            }
        }
    }

    /// Enter on the command line: `open LINK` (or `o LINK`, or a link
    /// alone) closes it and opens the link, an empty line just closes it;
    /// any other stays, saying why, as tuitube's (never quoting it).
    fn run_command(&mut self) {
        let Some(line) = self.command.as_mut() else {
            return;
        };
        let text = line.typed.text().trim();
        let (name, arg) = text
            .split_once(' ')
            .map_or((text, ""), |(name, arg)| (name, arg.trim()));
        let link = match name {
            "" => Ok(None),
            "open" | "o" if arg.is_empty() => Err("open needs a link"),
            "open" | "o" => links::parse(arg)
                .map(Some)
                .ok_or("Not a TikTok video's link"),
            _ => links::parse(text)
                .map(Some)
                .ok_or("No such command: open <link>"),
        };
        match link {
            Ok(link) => {
                self.command = None;
                if let Some(link) = link {
                    self.open_link(link);
                }
            }
            Err(why) => line.error = Some(why.into()),
        }
    }

    /// The theme `n`th in `theme::names()` in use, and saved at once; why
    /// not, if it can't be.
    fn pick_theme(&mut self, n: usize) -> Result<(), String> {
        let Some((id, _)) = theme::names().get(n) else {
            return Ok(());
        };
        self.set_theme(id);
        self.save_setting("theme", self.theme.into())
    }

    /// Pausing when the headphones are unplugged on or off, saved at once.
    /// Turned on, what plays now is watched too; off, nothing waits for
    /// them any more.
    fn toggle_pause_on_unplug(&mut self) -> Result<(), String> {
        self.pause_on_unplug = !self.pause_on_unplug;
        if self.pause_on_unplug {
            if self.loaded.is_some() {
                self.watch_output();
            }
        } else {
            self.held = None;
        }
        self.save_setting("pause_on_unplug", self.pause_on_unplug.into())
    }

    /// The animations on or off, saved at once.
    fn toggle_animations(&mut self) -> Result<(), String> {
        self.animations = !self.animations;
        self.save_setting("animations", self.animations.into())
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

    /// Keeps `key` as `value` in settings.toml, if there's a folder for it.
    fn save_setting(&self, key: &str, value: toml::Value) -> Result<(), String> {
        let Some(data) = &self.settings_in else {
            return Ok(());
        };
        settings::save(data, key, value).map_err(|e| text::one_line(&format!("{e:#}"), 200))
    }

    /// `act` on the player, while it shows the video shown.
    fn with_player(&self, act: impl Fn(&Player)) {
        if let Some(player) = &self.player
            && self.shows_picture()
        {
            act(player);
        }
    }

    /// Space: pauses or plays the video shown. While what loads waits for
    /// you since the headphones went (`held`), it plays on: the video shown
    /// loading too, or once it can load.
    fn toggle_pause(&mut self) {
        if self.shows_picture() {
            self.with_player(Player::toggle_pause);
            return;
        }
        if self.held.take().is_some()
            && self.loads_shown()
            && let Some(player) = &self.player
        {
            player.play();
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

    /// Pepper's full screen (`f`), in a Pepper that takes holes, while its
    /// picture's there: what plays can show over it, as TikTok's app shows
    /// the author and caption (an older Pepper's picture would cover it).
    fn has_controls(&self) -> bool {
        self.pepper_holes && self.fullscreen && self.picture_in_box()
    }

    /// Something was done: where there are controls (`has_controls`), they
    /// show for `CONTROLS_FOR`.
    fn show_controls(&mut self) {
        if self.has_controls() {
            self.controls_until = Some(Instant::now() + CONTROLS_FOR);
        }
    }

    /// Whether what plays shows over Pepper's full screen at `now`: where
    /// there are controls, for a while after anything's done
    /// (`show_controls`), and all the while it's paused.
    pub fn shows_controls(&self, now: Instant) -> bool {
        self.has_controls() && (self.paused || self.controls_until.is_some_and(|until| now < until))
    }

    /// How long until the controls hide, while they show for a while.
    fn controls_hide_in(&self, now: Instant) -> Option<Duration> {
        self.controls_until
            .map(|until| until.saturating_duration_since(now))
            .filter(|left| !left.is_zero() && self.has_controls())
    }

    /// Shows the video `by` from this one, within the feed. A key asks, so a
    /// player that ended by itself starts again.
    fn go(&mut self, by: isize) {
        self.player_gone = false;
        // You picked a video: it plays, the headphones gone or not.
        self.held = None;
        if self.videos.is_empty() {
            self.refill();
            return;
        }
        self.at = self.at.saturating_add_signed(by).min(self.videos.len() - 1);
        self.furthest = self.furthest.max(self.at);
        self.forget_behind();
        self.prune();
        self.prefetch();
        self.want_covers();
        self.want_comments();
        self.play_current();
        self.refill();
    }

    /// Forgets the videos more than `KEPT_BEHIND` before the one shown:
    /// they count as watched, and `k` goes back no further.
    fn forget_behind(&mut self) {
        let gone = self.at.saturating_sub(KEPT_BEHIND);
        if gone == 0 {
            return;
        }
        for video in self.videos.drain(..gone) {
            self.failures.remove(&video.id);
            if let Some(Download::Ready(path)) = self.downloads.remove(&video.id) {
                let _ = std::fs::remove_file(path);
            }
            self.watched_before.push(video.id);
        }
        self.at -= gone;
        self.furthest -= gone;
        self.batch_from = self.batch_from.saturating_sub(gone);
    }

    /// Fetches more if the feed shown wants more now.
    fn refill(&mut self) {
        if self.wants_more() {
            self.fetch_more();
        }
    }

    /// Whether the feed shown wants more now: few videos are left after the
    /// one shown, or For You's first batches are still growing; and no
    /// fetch failed lately (then it waits `RETRY_AFTER`: `AppEvent::Rested`).
    fn wants_more(&self) -> bool {
        let left = self.videos.len().saturating_sub(self.at + 1);
        let growing = self.category.is_none() && self.ramping();
        let resting = self.failed_at.is_some_and(|at| at.elapsed() < RETRY_AFTER);
        (left <= REFILL_AT || growing) && !resting
    }
}

/// `pasted` put in a line being typed, at its cursor, as one line
/// (`text::one_line`), up to `max` characters in all; a space it had at
/// either end is kept where a word would run into it otherwise
/// (`Typed::paste`).
fn paste_into(line: &mut Typed, pasted: &str, max: usize) {
    // A paste can be any size: cut before it's cleaned. Its line breaks
    // may come as \r, a control character `clean` drops: a space between
    // the lines, as \n is.
    let clean = text::clean(&text::first_chars(pasted, 4 * max).replace('\r', "\n"));
    let spaced = (
        clean.starts_with(char::is_whitespace),
        clean.ends_with(char::is_whitespace),
    );
    line.paste(&text::one_line(&clean, max), spaced, max);
}

/// Ctrl-C: tuitok quits, whatever is open.
fn ctrl_c(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
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
/// cells (`TIOCGWINSZ`), if it says.
fn cell_size() -> Option<(u16, u16)> {
    match crossterm::terminal::window_size() {
        Ok(s) if s.columns > 0 && s.rows > 0 && s.width > 0 && s.height > 0 => {
            Some((s.width / s.columns, s.height / s.rows))
        }
        _ => None,
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

    /// Download `number` of video `id` under way, as `prefetch` starts one
    /// (a task that never ends stands in for it).
    fn under_way(app: &mut App, id: &str, number: u64) {
        let id = VideoId::new(id).unwrap();
        let abort = tokio::spawn(std::future::pending::<()>()).abort_handle();
        let getting = Download::Getting { got: 0, of: None };
        app.downloads.insert(id.clone(), getting);
        app.underway.insert(id, Underway { number, abort });
        app.downloads_started = app.downloads_started.max(number);
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

    #[tokio::test]
    async fn downloads_are_planned_nearest_first_and_big_ones_stream() {
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
                    Step::Start(number, job) => format!("start {} #{number}", job.id.as_str()),
                    Step::Mark(id, Download::Streams) => format!("stream {}", id.as_str()),
                    Step::Mark(id, _) => format!("skip {}", id.as_str()),
                })
                .collect::<Vec<_>>()
        };
        // Two at a time: the first, the second marked to stream, the third;
        // the fourth, with nothing that plays, takes no turn.
        assert_eq!(
            plan(&app),
            ["start 1 #1", "stream 2", "start 3 #2", "skip 4"]
        );
        let job_path = match app.plan().remove(0) {
            Step::Start(_, job) => job.path,
            Step::Mark(..) => unreachable!(),
        };
        assert_eq!(
            job_path,
            PathBuf::from("/nowhere/cache/1-1-h265-720x1280.mp4")
        );
        // With those under way, the rest come as they finish.
        under_way(&mut app, "1", 1);
        app.downloads
            .insert(VideoId::new("2").unwrap(), Download::Streams);
        under_way(&mut app, "3", 2);
        assert_eq!(plan(&app), ["skip 4"]);
        app.on_app_event(AppEvent::Downloaded {
            id: VideoId::new("1").unwrap(),
            number: 1,
            outcome: Download::Ready("/nowhere/cache/1.mp4".into()),
        });
        assert!(matches!(
            app.downloads[&VideoId::new("1").unwrap()],
            Download::Ready(_)
        ));
        // Only the video shown and the three after it.
        app.at = 2;
        app.downloads
            .insert(VideoId::new("4").unwrap(), Download::Failed("x".into()));
        assert_eq!(plan(&app), ["start 5 #3"]);
        // A download of a video now behind the one shown stops, and its turn
        // goes to one ahead.
        app.at = 3;
        app.prefetch();
        let three = VideoId::new("3").unwrap();
        assert!(app.underway.is_empty() && !app.downloads.contains_key(&three));
        assert_eq!(plan(&app), ["start 5 #3", "start 6 #4"]);
    }

    #[tokio::test]
    async fn the_video_shown_streams_if_its_download_keeps_it_waiting() {
        let mut app = app_with(["1", "2", "3"].map(crate::ui::tests::video).to_vec());
        let id = |n: &str| VideoId::new(n).unwrap();
        // 1's download is under way: it waits, then streams, and the
        // download stops.
        under_way(&mut app, "1", 1);
        app.on_app_event(AppEvent::Waited(id("1")));
        assert!(app.underway.is_empty());
        assert_eq!(app.downloads[&id("1")], Download::Streams);
        // 2's waits its turn behind two others: it streams too.
        app.downloads.clear();
        app.at = 1;
        under_way(&mut app, "3", 2);
        under_way(&mut app, "4", 3);
        assert!(!app.plan().iter().any(|s| matches!(s, Step::Start(..))));
        app.on_app_event(AppEvent::Waited(id("2")));
        assert_eq!(app.downloads[&id("2")], Download::Streams);
        // One not shown any more, or that can play by now, is left as it is.
        app.on_app_event(AppEvent::Waited(id("3")));
        assert!(matches!(app.downloads[&id("3")], Download::Getting { .. }));
        app.at = 2;
        app.downloads
            .insert(id("3"), Download::Ready("/nowhere/cache/3.mp4".into()));
        app.on_app_event(AppEvent::Waited(id("3")));
        assert!(matches!(app.downloads[&id("3")], Download::Ready(_)));
    }

    #[tokio::test]
    async fn progress_updates_a_download_under_way() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        let id = VideoId::new("1").unwrap();
        under_way(&mut app, "1", 4);
        let progress = |number, got| AppEvent::Progress {
            id: id.clone(),
            number,
            got,
            of: Some(1_000),
        };
        app.on_app_event(progress(4, 300));
        // A download stopped before says nothing of this one.
        app.on_app_event(progress(3, 900));
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
        assert_eq!(app.placement(), pepper::place("t0k", area, &[]));
        assert_eq!(app.placement(), "");
        // The window grew: placed again.
        app.size = (150, 50);
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area(), &[]));
        // Another video shown, not playing yet: taken away.
        app.at = 1;
        assert_eq!(app.placement(), pepper::remove("t0k"));
        assert_eq!(app.placement(), "");
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[tokio::test]
    async fn a_failed_download_is_tried_again_unless_it_streams_meanwhile() {
        let mut app = app_with(vec![
            crate::ui::tests::video("1"),
            crate::ui::tests::video("2"),
        ]);
        let (one, two) = (VideoId::new("1").unwrap(), VideoId::new("2").unwrap());
        let failed = |number| AppEvent::Downloaded {
            id: two.clone(),
            number,
            outcome: Download::Failed("HTTP 503".into()),
        };
        app.downloads
            .insert(one.clone(), Download::Ready("/nowhere/cache/1.mp4".into()));
        under_way(&mut app, "2", 1);
        app.on_app_event(failed(1));
        assert_eq!(app.failures[&two], 1);
        // Its wait is over: downloaded again.
        app.on_app_event(AppEvent::TryAgain(two.clone()));
        assert!(!app.downloads.contains_key(&two));
        assert!(matches!(&app.plan()[..], [Step::Start(2, job)] if job.id == two));
        under_way(&mut app, "2", 2);
        app.on_app_event(failed(2));
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
    fn an_error_is_the_video_shown_s_own() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        app.loaded = Some(VideoId::new("1").unwrap());
        app.play_error = Some("It can't play: loading failed".into());
        // 2 can't play yet: 1's error isn't shown on it.
        app.downloads.insert(
            VideoId::new("2").unwrap(),
            Download::Getting { got: 0, of: None },
        );
        press(&mut app, KeyCode::Char('j'));
        assert_eq!((app.at, app.play_error.as_deref()), (1, None));
    }

    #[test]
    fn a_closed_player_opens_again_only_when_a_key_asks() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        for n in ["1", "2"] {
            let path = PathBuf::from(format!("/nowhere/cache/{n}.mp4"));
            app.downloads
                .insert(VideoId::new(n).unwrap(), Download::Ready(path));
        }
        app.players = 1;
        app.loaded = Some(VideoId::new("1").unwrap());
        app.on_app_event(AppEvent::Player {
            player: 1,
            event: PlayerEvent::Exited,
        });
        assert!(app.player_gone && app.loaded.is_none());
        // A download ending (or a batch) doesn't open it again.
        app.play_current();
        assert!(app.loaded.is_none() && app.play_error.is_none());
        // j does (here there's none to start, so it says so).
        press(&mut app, KeyCode::Char('j'));
        assert!(!app.player_gone);
        assert_eq!(app.play_error.as_deref(), Some(NO_PLAYER));
    }

    #[test]
    fn a_failed_fetch_is_tried_again_once_its_wait_is_over() {
        let mut app = app_with(Vec::new());
        app.on_batch(Err("Lightpanda: no".into()));
        assert!(!app.wants_more(), "it waits");
        // Its wait is over (`AppEvent::Rested`): an empty feed wants more.
        app.failed_at = Instant::now().checked_sub(RETRY_AFTER);
        assert!(app.wants_more());
        // So does one whose first batches are still growing, but not one
        // with plenty left after the video shown.
        let mut app = app_with(
            (1..=13)
                .map(|n| crate::ui::tests::video(&n.to_string()))
                .collect(),
        );
        assert!(app.wants_more(), "still growing");
        app.batches = feed::SIZES.len();
        assert!(!app.wants_more());
        app.at = 12;
        assert!(app.wants_more());
        // A category's page with nothing new waits as a failure does.
        app.category = Some(explore::CATEGORIES[2]);
        app.on_explored(explore::CATEGORIES[2], page(&["1"]));
        assert!(app.failed_at.is_some() && !app.wants_more());
    }

    #[test]
    fn videos_far_behind_are_forgotten_and_count_as_watched() {
        let videos = (1..=KEPT_BEHIND + 5)
            .map(|n| crate::ui::tests::video(&n.to_string()))
            .collect();
        let mut app = app_with(videos);
        for _ in 0..KEPT_BEHIND + 2 {
            app.go(1);
        }
        assert_eq!((app.at, app.videos.len()), (KEPT_BEHIND, KEPT_BEHIND + 3));
        assert_eq!(app.videos[0].id.as_str(), "3");
        let watched = app.carry_out().watched;
        assert_eq!((watched.len(), watched[0].as_str()), (KEPT_BEHIND + 3, "1"));
        // TikTok sends 1 again: it isn't shown.
        app.add(vec![crate::ui::tests::video("1")]);
        assert_eq!(app.videos.len(), KEPT_BEHIND + 3);
    }

    #[test]
    fn a_resize_keeps_the_cell_size_found_when_the_terminal_doesnt_say() {
        let mut app = app_with(Vec::new());
        app.cell = (10, 20);
        app.on_event(Event::Resize(100, 30));
        assert_eq!(app.cell, cell_size().unwrap_or((10, 20)));
        assert_eq!(app.size, (100, 30));
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
            theme::names()[menu.selected - HelpMenu::THEMES].0,
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
            HelpMenu::THEMES + theme::names().len() - 1
        );
        // The settings above the themes: on or off, saved at once.
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(
            app.help.as_ref().unwrap().selected,
            HelpMenu::PAUSE_ON_UNPLUG
        );
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char(' '));
        assert!(!app.pause_on_unplug && !app.animations);
        let saved = settings::load(&data);
        assert!(!saved.pause_on_unplug && !saved.animations);
        assert_eq!(saved.theme, before_nord, "the rest kept");
        // q closes it, and doesn't quit; it kept what was picked.
        press(&mut app, KeyCode::Char('q'));
        assert!(app.help.is_none() && !app.quit);
        assert_eq!(app.theme, before_nord);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn what_plays_pauses_when_the_headphones_are_unplugged() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        let gone = |app: &mut App| app.on_app_event(AppEvent::OutputGone("AirPods".into()));
        // Off: nothing.
        app.pause_on_unplug = false;
        set_playing(&mut app, "1", true);
        gone(&mut app);
        assert_eq!(app.held, None, "off");
        app.pause_on_unplug = true;
        gone(&mut app);
        assert_eq!(app.held.as_deref(), Some("AirPods"));
        // Paused, then played by you (space, or mpv's window): nothing waits.
        app.on_player(PlayerEvent::Paused(true));
        app.on_player(PlayerEvent::Paused(false));
        assert_eq!(app.held, None);
        // Paused already, or about to play (mpv not yet said so): paused,
        // and what loads next waits too.
        app.on_player(PlayerEvent::Paused(true));
        gone(&mut app);
        assert!(app.held.is_some());
        // j: you picked the next, which plays.
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.held, None);
        // Gone while the one shown loads: it starts paused, until space.
        app.loaded = None;
        app.playing = None;
        gone(&mut app);
        assert!(app.held.is_some());
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.held, None, "space plays on");
        // With nothing shown yet (the first batch, a category picked), what
        // comes first waits.
        let mut empty = app_with(Vec::new());
        gone(&mut empty);
        assert!(empty.held.is_some());
    }

    #[test]
    fn the_volume_goes_by_5_unmutes_and_is_kept_when_it_changed() {
        let mut app = app_with(Vec::new());
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/test-app-volume-{}", std::process::id()));
        crate::config::private_dir(&data).unwrap();
        let at = |volume| Settings {
            volume,
            ..Settings::default()
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

    fn page(ids: &[&str]) -> Result<explore::Page, String> {
        Ok(explore::Page {
            videos: ids.iter().map(|id| crate::ui::tests::video(id)).collect(),
            guest: explore::Guest::made_up("tok"),
            took: Duration::from_millis(600),
        })
    }

    fn ids(app: &App) -> Vec<&str> {
        app.videos.iter().map(|v| v.id.as_str()).collect()
    }

    #[test]
    fn slash_opens_the_bar_and_typing_narrows_it() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        set_playing(&mut app, "1", true);
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area(), &[]));
        press(&mut app, KeyCode::Char('/'));
        assert_eq!(app.search, Some(Search::default()), "the cursor on For You");
        // Its box is over the panel: the video plays on beside it.
        assert_eq!(app.placement(), "");
        // With no panel (a narrow window) it's over the video: taken away.
        app.size = (80, 40);
        assert!(app.offers_cover_video());
        assert_eq!(app.placement(), pepper::remove("t0k"));
        // Typing puts the cursor on the best match but For You, shown.
        for c in "fo".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.search.as_ref().unwrap().selected, 1, "Food");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        // While it's open, keys are typed: j doesn't move, q doesn't quit.
        for c in "jq".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!((app.at, app.quit), (0, false));
        assert_eq!(app.search.as_ref().unwrap().typed.text(), "jq");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "com".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Char('\u{1b}'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        let search = app.search.as_ref().unwrap();
        assert_eq!(
            (search.typed.text(), search.selected),
            ("com", 1),
            "Comedy, Anime & Comics"
        );
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.search.as_ref().unwrap().selected, 0);
        // Esc closes it and nothing changes; the picture comes back.
        press(&mut app, KeyCode::Esc);
        assert!(app.search.is_none() && !app.quit);
        assert_eq!((app.category, app.videos.len()), (None, 2));
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area(), &[]));
    }

    #[tokio::test]
    async fn a_category_picked_takes_the_place_of_for_you_from_its_start() {
        let mut app = app_with(["1", "2", "3"].map(crate::ui::tests::video).to_vec());
        let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/test-explore-{}.mp4", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        app.downloads
            .insert(VideoId::new("1").unwrap(), Download::Ready(file.clone()));
        app.go(1);
        set_playing(&mut app, "2", false);
        under_way(&mut app, "3", 1);
        for key in [
            KeyCode::Char('/'),
            KeyCode::Char('c'),
            KeyCode::Char('o'),
            KeyCode::Char('m'),
        ] {
            press(&mut app, key);
        }
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(app.search.is_none());
        assert_eq!(app.feed_name(), "Anime & Comics");
        assert_eq!(app.feed, FeedState::Exploring);
        assert!(app.videos.is_empty() && app.at == 0 && app.downloads.is_empty());
        assert!(!file.exists(), "the old feed's downloads go");
        assert!(app.underway.is_empty(), "and stop");
        assert!(app.loaded.is_none() && !app.shows_picture());
        // What was shown counts as watched: 1 and 2, not 3.
        let watched: Vec<String> = app
            .carry_out()
            .watched
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        assert_eq!(watched, ["1", "2"]);
        // For You's videos are dropped while a category is shown; its
        // batch's guest goes on.
        app.fetching = true;
        app.on_app_event(AppEvent::Found(vec![crate::ui::tests::video("4")]));
        app.on_app_event(AppEvent::Batch(Ok(batch(&["4"], &[]))));
        assert!(app.videos.is_empty() && !app.fetching);
        assert_eq!(app.cookies.len(), 1);
        // A page of another category is dropped, but its guest is kept; the
        // category's own is added, less what was seen.
        let comedy = explore::CATEGORIES[2];
        app.on_app_event(AppEvent::Explored {
            category: comedy,
            page: page(&["5"]),
        });
        assert!(app.videos.is_empty() && app.explore_guest.is_some());
        let anime = explore::CATEGORIES[4];
        app.on_app_event(AppEvent::Explored {
            category: anime,
            page: page(&["6", "7", "1"]),
        });
        assert_eq!(ids(&app), ["6", "7"]);
        assert_eq!(
            app.feed,
            FeedState::Explored {
                new: 2,
                took: Duration::from_millis(600)
            }
        );
        // The end of one stopped, if it came all the same, is dropped.
        std::fs::write(&file, b"x").unwrap();
        app.on_app_event(AppEvent::Downloaded {
            id: VideoId::new("3").unwrap(),
            number: 1,
            outcome: Download::Ready(file.clone()),
        });
        assert!(!file.exists() && app.underway.is_empty());
        assert!(!app.downloads.contains_key(&VideoId::new("3").unwrap()));
        // A page that fails says so; the guest goes.
        app.on_app_event(AppEvent::Explored {
            category: anime,
            page: Err("TikTok sent nothing".into()),
        });
        assert_eq!(app.feed, FeedState::Failed("TikTok sent nothing".into()));
        assert!(app.explore_guest.is_none() && app.videos.len() == 2);
        // `/` again: the cursor on the category shown. For You comes back
        // from its start.
        press(&mut app, KeyCode::Char('/'));
        assert_eq!(app.search.as_ref().unwrap().selected, 1 + 4);
        for c in "for".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            (app.category, app.feed.clone()),
            (None, FeedState::NoBrowser)
        );
        assert!(app.videos.is_empty());
        app.on_app_event(AppEvent::Found(vec![crate::ui::tests::video("8")]));
        assert_eq!(ids(&app), ["8"]);
    }

    #[test]
    fn the_popup_takes_the_picture_away_in_a_pepper_without_holes() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        set_playing(&mut app, "1", true);
        let area = app.video_area();
        assert_eq!(app.placement(), pepper::place("t0k", area, &[]));
        press(&mut app, KeyCode::Char('?'));
        assert!(!app.picture_in_box());
        assert_eq!(app.placement(), pepper::remove("t0k"));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area(), &[]));
    }

    #[test]
    fn in_peppers_full_screen_what_plays_shows_a_while_after_anything() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        set_playing(&mut app, "1", true);
        let key = |app: &mut App, c| {
            app.on_event(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        };
        // Not in its box, or a Pepper without holes: nothing.
        app.pepper = true;
        key(&mut app, 'f');
        assert!(app.fullscreen && app.controls_until.is_none());
        app.pepper_holes = true;
        key(&mut app, 'm');
        let now = Instant::now();
        assert!(app.shows_controls(now));
        let hide_in = app.controls_hide_in(now).unwrap();
        assert!(hide_in <= CONTROLS_FOR && hide_in > CONTROLS_FOR / 2);
        assert!(!app.shows_controls(now + CONTROLS_FOR), "gone in 3 s");
        // Paused: all the while; and a pause or play shows them again.
        app.controls_until = None;
        app.on_player(PlayerEvent::Paused(true));
        assert!(app.controls_until.is_some());
        assert!(app.shows_controls(now + 2 * CONTROLS_FOR));
        // A new video's first frame shows them.
        app.on_player(PlayerEvent::Paused(false));
        app.controls_until = None;
        (app.loaded, app.entry) = (Some(VideoId::new("2").unwrap()), Some(7));
        app.at = 1;
        app.on_player(PlayerEvent::Restarted(7));
        assert!(app.controls_until.is_some());
        // Out of full screen: none.
        key(&mut app, 'f');
        assert!(!app.shows_controls(now));
    }

    #[test]
    fn where_pepper_takes_holes_the_picture_plays_on_around_a_popup() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        set_playing(&mut app, "1", true);
        app.pepper_holes = true;
        let area = app.video_area();
        assert_eq!(app.placement(), pepper::place("t0k", area, &[]));
        // The `?` popup: a hole where it's over the box, cut to it.
        press(&mut app, KeyCode::Char('?'));
        assert!(app.picture_in_box());
        let popup = Rect::new(area.x - 5, area.y + 2, area.width + 10, 6);
        app.drawn_over.replace(vec![popup]);
        let hole = Rect::new(area.x, area.y + 2, area.width, 6);
        assert_eq!(app.placement(), pepper::place("t0k", area, &[hole]));
        assert_eq!(app.placement(), "", "said once");
        // Closed: whole again.
        press(&mut app, KeyCode::Esc);
        app.drawn_over.replace(Vec::new());
        assert_eq!(app.placement(), pepper::place("t0k", area, &[]));
        // `/`'s box over the video's side, with no panel: it plays on too.
        app.size = (80, 40);
        press(&mut app, KeyCode::Char('/'));
        assert!(app.offers_cover_video() && app.picture_in_box());
    }

    #[test]
    fn the_players_words() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        let said = |app: &mut App, event| {
            app.on_app_event(AppEvent::Player { player: 2, event });
        };
        app.players = 2;
        (app.loaded, app.load) = (Some(VideoId::new("1").unwrap()), 1);
        said(&mut app, PlayerEvent::Loaded { load: 1, entry: 1 });
        said(&mut app, PlayerEvent::Restarted(1));
        assert!(app.shows_picture());
        said(&mut app, PlayerEvent::Paused(true));
        said(&mut app, PlayerEvent::Position(1, 3.0));
        assert_eq!((app.paused, app.position), (true, 3.0));
        // An older player's words are dropped.
        app.on_app_event(AppEvent::Player {
            player: 1,
            event: PlayerEvent::Exited,
        });
        assert!(app.shows_picture());
        // The next video is loaded (as `play_current` does): what the one
        // before says as it loops or ends meanwhile isn't taken for its.
        app.at = 1;
        (app.loaded, app.load, app.entry) = (Some(VideoId::new("2").unwrap()), 2, None);
        (app.playing, app.position) = (None, 0.0);
        said(&mut app, PlayerEvent::Restarted(1));
        said(&mut app, PlayerEvent::Loaded { load: 2, entry: 2 });
        said(&mut app, PlayerEvent::Position(1, 9.0));
        said(&mut app, PlayerEvent::Failed(1, "loading failed".into()));
        assert!(!app.shows_picture());
        assert_eq!((app.position, app.play_error.as_deref()), (0.0, None));
        said(&mut app, PlayerEvent::Restarted(2));
        assert!(app.shows_picture());
        said(&mut app, PlayerEvent::Failed(2, "loading failed".into()));
        assert!(!app.shows_picture());
        assert_eq!(
            app.play_error.as_deref(),
            Some("It can't play: loading failed")
        );
        said(&mut app, PlayerEvent::Exited);
        assert!(app.loaded.is_none() && app.token.is_none() && app.player_gone);
    }

    fn type_in(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn colon_opens_the_command_line_and_open_takes_a_link() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        set_playing(&mut app, "1", true);
        press(&mut app, KeyCode::Char(':'));
        assert_eq!(app.command, Some(CommandLine::default()));
        assert_eq!(app.command_offers(), [&COMMANDS[0]]);
        // Its box is over the panel: the video plays on beside it; with no
        // panel, over the video, which is taken away.
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area(), &[]));
        app.size = (80, 40);
        assert_eq!(app.placement(), pepper::remove("t0k"));
        // While it's open, keys are typed: j doesn't move, q doesn't quit.
        type_in(&mut app, "jq");
        assert_eq!((app.at, app.quit), (0, false));
        assert!(app.command_offers().is_empty());
        let error = |app: &App| app.command.as_ref().unwrap().error.clone();
        press(&mut app, KeyCode::Enter);
        assert_eq!(error(&app).as_deref(), Some("No such command: open <link>"));
        // Editing clears why; Tab completes the command, and its box goes.
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(error(&app), None);
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.command.as_ref().unwrap().typed.text(), "open ");
        assert_eq!(app.placement(), pepper::place("t0k", app.video_area(), &[]));
        press(&mut app, KeyCode::Enter);
        assert_eq!(error(&app).as_deref(), Some("open needs a link"));
        type_in(
            &mut app,
            "https://www.tiktok.com/@x/photo/7657182246486478098",
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(error(&app).as_deref(), Some("Not a TikTok video's link"));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.command.as_ref().unwrap().typed.text(), "open ");
        type_in(&mut app, "vt.tiktok.com/ZSe4FqkKd");
        press(&mut app, KeyCode::Enter);
        assert!(app.command.is_none());
        assert_eq!(
            (app.link.clone(), app.links_opened),
            (Some(LinkState::Opening), 1)
        );
        // A link alone opens too, `o` is `open`, and an empty line just
        // closes.
        for line in ["7657182246486478098", "o 7657182246486478098"] {
            press(&mut app, KeyCode::Char(':'));
            type_in(&mut app, line);
            press(&mut app, KeyCode::Enter);
        }
        assert_eq!(app.links_opened, 3);
        press(&mut app, KeyCode::Char(':'));
        press(&mut app, KeyCode::Enter);
        assert!(app.command.is_none() && app.links_opened == 3);
        press(&mut app, KeyCode::Char(':'));
        press(&mut app, KeyCode::Esc);
        assert!(app.command.is_none() && !app.quit);
    }

    #[test]
    fn an_opened_video_plays_next_and_the_feed_goes_on_after_it() {
        let mut app = app_with(["1", "2", "3"].map(crate::ui::tests::video).to_vec());
        app.go(1);
        // 3 is of the batch being fetched.
        app.batch_from = 2;
        let open = |app: &mut App, id: &str| {
            app.open_link(Link::Video(VideoId::new(id).unwrap()));
            let number = app.links_opened;
            app.on_app_event(AppEvent::Opened {
                number,
                video: Ok(crate::ui::tests::video(id)),
            });
        };
        app.open_link(Link::Video(VideoId::new("9").unwrap()));
        assert_eq!(app.link, Some(LinkState::Opening));
        // A link opened before it: dropped.
        app.on_app_event(AppEvent::Opened {
            number: 0,
            video: Ok(crate::ui::tests::video("8")),
        });
        assert_eq!(ids(&app), ["1", "2", "3"]);
        app.on_app_event(AppEvent::Opened {
            number: 1,
            video: Ok(crate::ui::tests::video("9")),
        });
        assert_eq!(ids(&app), ["1", "2", "9", "3"]);
        assert_eq!((app.at, app.found_so_far(), app.link.clone()), (2, 1, None));
        // The feed doesn't send it again, and it counts as watched.
        app.add(vec![crate::ui::tests::video("9")]);
        assert_eq!(app.videos.len(), 4);
        let watched = |app: &App| -> Vec<String> {
            let out = app.carry_out().watched;
            out.iter().map(|id| id.as_str().to_owned()).collect()
        };
        assert_eq!(watched(&app), ["1", "2", "9"]);
        // One in the feed already is gone back to, not added again.
        open(&mut app, "1");
        assert_eq!((app.videos.len(), app.at), (4, 0));
        // Gone back, one goes after the video shown; the furthest shown
        // stays the same video.
        open(&mut app, "7");
        assert_eq!(ids(&app), ["1", "7", "2", "9", "3"]);
        assert_eq!(app.at, 1);
        assert_eq!(watched(&app), ["1", "7", "2", "9"]);
        // One ahead is moved after the video shown: the ones between it and
        // the one shown aren't skipped, nor counted as watched.
        let mut ahead = app_with(
            ["1", "2", "3", "4", "5"]
                .map(crate::ui::tests::video)
                .to_vec(),
        );
        open(&mut ahead, "4");
        assert_eq!(ids(&ahead), ["1", "4", "2", "3", "5"]);
        assert_eq!(
            (ahead.at, watched(&ahead)),
            (1, vec!["1".into(), "4".into()])
        );
        // One that fails says why, until the next key, typed in a line too.
        app.open_link(Link::Video(VideoId::new("6").unwrap()));
        app.on_app_event(AppEvent::Opened {
            number: app.links_opened,
            video: Err("the video is private".into()),
        });
        let failed = "The link failed: the video is private";
        assert_eq!(app.link, Some(LinkState::Failed(failed.into())));
        press(&mut app, KeyCode::Char(':'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.link, None);
    }

    #[test]
    fn a_link_s_video_comes_first_in_an_empty_feed() {
        let mut app = app_with(Vec::new());
        app.open_first(Link::Video(VideoId::new("9").unwrap()));
        let link = app.first_link.take().unwrap();
        app.open_link(link);
        app.on_app_event(AppEvent::Opened {
            number: 1,
            video: Ok(crate::ui::tests::video("9")),
        });
        assert_eq!((ids(&app), app.at, app.found_so_far()), (vec!["9"], 0, 0));
        app.on_app_event(AppEvent::Found(
            ["1", "2"].map(crate::ui::tests::video).to_vec(),
        ));
        assert_eq!((ids(&app), app.found_so_far()), (vec!["9", "1", "2"], 2));
        // A link to a video watched in a run before: it plays, and is kept
        // as watched once.
        let mut app = app_with(Vec::new());
        app.carry_in(crate::guest::Guest {
            cookies: Vec::new(),
            watched: vec![VideoId::new("9").unwrap(), VideoId::new("8").unwrap()],
        });
        app.open_link(Link::Video(VideoId::new("9").unwrap()));
        app.on_app_event(AppEvent::Opened {
            number: 1,
            video: Ok(crate::ui::tests::video("9")),
        });
        assert_eq!(ids(&app), ["9"]);
        let watched = app.carry_out().watched;
        let watched: Vec<&str> = watched.iter().map(VideoId::as_str).collect();
        assert_eq!(watched, ["8", "9"]);
    }

    #[test]
    fn keys_under_the_splash_screen_only_take_it_away_or_quit() {
        let mut app = app_with(["1", "2"].map(crate::ui::tests::video).to_vec());
        let key = |app: &mut App, c: char| {
            app.on_event(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        };
        app.keep_splash(Instant::now());
        key(&mut app, 'j');
        assert_eq!(app.at, 0);
        assert!(app.splash.is_some(), "fading out");
        // Once it fades, keys work.
        key(&mut app, 'j');
        assert_eq!(app.at, 1);
        // A paste takes it away too, and opens nothing.
        let mut pasted = app_with(Vec::new());
        pasted.keep_splash(Instant::now());
        pasted.on_event(Event::Paste("7657182246486478098".into()));
        assert_eq!(pasted.links_opened, 0);
        // q quits.
        let mut quitting = app_with(Vec::new());
        quitting.keep_splash(Instant::now());
        key(&mut quitting, 'q');
        assert!(quitting.quit);
    }

    #[test]
    fn the_lines_edit_at_their_cursor() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        let typed = |app: &App| app.command.as_ref().unwrap().typed.text().to_owned();
        press(&mut app, KeyCode::Char(':'));
        type_in(&mut app, "opn");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(typed(&app), "open");
        // A paste goes in at the cursor too, apart from the word after it.
        press(&mut app, KeyCode::Home);
        app.on_event(Event::Paste("x ".into()));
        assert_eq!(typed(&app), "x open");
        press(&mut app, KeyCode::Esc);
        // `/`: ← and → move the cursor, not the row under it.
        press(&mut app, KeyCode::Char('/'));
        type_in(&mut app, "fd");
        let selected = app.search.as_ref().unwrap().selected;
        press(&mut app, KeyCode::Left);
        assert_eq!(app.search.as_ref().unwrap().selected, selected);
        type_in(&mut app, "oo");
        let search = app.search.as_ref().unwrap();
        assert_eq!(search.typed.text(), "food");
        assert_eq!(
            explore::name(explore::matching("food")[search.selected]),
            "Food"
        );
    }

    #[test]
    fn a_paste_goes_into_the_line_open_else_a_link_opens() {
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        let paste = |app: &mut App, text: &str| app.on_event(Event::Paste(text.into()));
        // Nothing open: a link opens; anything else is said not to be one,
        // but not over a link still being opened.
        paste(&mut app, "hello there");
        assert!(matches!(app.link, Some(LinkState::Failed(_))));
        paste(&mut app, "https://vt.tiktok.com/ZSe4FqkKd\n");
        assert_eq!(
            (app.link.clone(), app.links_opened),
            (Some(LinkState::Opening), 1)
        );
        paste(&mut app, "hello there");
        assert_eq!(app.link, Some(LinkState::Opening));
        assert_eq!(app.links_opened, 1);
        // Into the command line, as one line, cleaned and capped; a space
        // it starts with stays.
        press(&mut app, KeyCode::Char(':'));
        type_in(&mut app, "open");
        paste(&mut app, " a\nb\u{1b}c");
        assert_eq!(app.command.as_ref().unwrap().typed.text(), "open a bc");
        paste(&mut app, &"x".repeat(3 * MAX_COMMAND));
        assert_eq!(
            app.command.as_ref().unwrap().typed.text().len(),
            MAX_COMMAND
        );
        press(&mut app, KeyCode::Esc);
        // Lines pasted, their breaks \r as many terminals send them: one
        // line, a space between.
        press(&mut app, KeyCode::Char(':'));
        paste(&mut app, "open\rhttps://vt.tiktok.com/ZSe4FqkKd");
        assert_eq!(
            app.command.as_ref().unwrap().typed.text(),
            "open https://vt.tiktok.com/ZSe4FqkKd"
        );
        press(&mut app, KeyCode::Esc);
        // Into `/`'s, narrowing it.
        press(&mut app, KeyCode::Char('/'));
        paste(&mut app, "foo");
        let search = app.search.as_ref().unwrap();
        assert_eq!(search.typed.text(), "foo");
        assert_eq!(
            explore::name(explore::matching("foo")[search.selected]),
            "Food"
        );
        press(&mut app, KeyCode::Esc);
        // Not while the popup is open.
        press(&mut app, KeyCode::Char('?'));
        paste(&mut app, "7657182246486478098");
        assert_eq!(app.links_opened, 1);
        // The demo opens none.
        let mut demo = App::demo(false);
        paste(&mut demo, "7657182246486478098");
        let none = "The demo opens no links";
        assert_eq!(demo.link, Some(LinkState::Failed(none.into())));
    }
}
