//! What tuitok holds, and its event loop: one `tokio::select!` over the
//! terminal's events and tuitok's own (a batch of the feed arriving), a
//! redraw after each.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::browser::Engine;
use crate::feed::{self, Batch};
use crate::text;
use crate::theme::{self, Colors};
use crate::ui;
use crate::video::{Video, VideoId};

/// A cell's size in pixels when the terminal doesn't say: about what most
/// fonts make, so the box is still about upright.
const GUESSED_CELL: (u16, u16) = (9, 18);

/// More of the feed is fetched once this few videos are left after the one
/// shown.
const REFILL_AT: usize = 5;

/// After a batch fails, how long before one is tried again on its own (`r`
/// tries at once).
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// What tuitok's own tasks tell the event loop.
pub enum AppEvent {
    Batch(Result<Batch, String>),
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

pub struct App {
    pub colors: Colors,
    /// tuitok runs in Pepper (`pepper::port`): videos will play in the box.
    pub pepper: bool,
    /// A cell's size in pixels, for the box's shape.
    pub cell: (u16, u16),
    /// The feed so far, each video once.
    pub videos: Vec<Video>,
    /// The video shown.
    pub at: usize,
    pub feed: FeedState,
    engines: Vec<(Engine, PathBuf)>,
    profile: PathBuf,
    seen: HashSet<VideoId>,
    fetching: bool,
    failed_at: Option<Instant>,
    /// Where tasks send `AppEvent`s, once `run` has started.
    tx: Option<mpsc::UnboundedSender<AppEvent>>,
    quit: bool,
}

impl App {
    /// `engines` are the browsers to fetch the feed with, in order
    /// (`feed::engines`); `profile` is where Chrome keeps its profile.
    pub fn new(pepper: bool, engines: Vec<(Engine, PathBuf)>, profile: PathBuf) -> Self {
        let feed = match engines.first() {
            Some(&(engine, _)) => FeedState::Fetching(engine),
            None => FeedState::NoBrowser,
        };
        Self {
            colors: Colors::named(theme::DEFAULT),
            pepper,
            cell: cell_size(),
            videos: Vec::new(),
            at: 0,
            feed,
            engines,
            profile,
            seen: HashSet::new(),
            fetching: false,
            failed_at: None,
            tx: None,
            quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let (tx, mut app_events) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        self.fetch_more();
        let mut terminal_events = EventStream::new();
        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, self))?;
            tokio::select! {
                event = terminal_events.next() => match event {
                    Some(Ok(event)) => self.on_event(event),
                    Some(Err(e)) => return Err(e),
                    None => break,
                },
                Some(event) = app_events.recv() => self.on_app_event(event),
            }
        }
        Ok(())
    }

    /// The video shown, if there is one.
    pub fn current(&self) -> Option<&Video> {
        self.videos.get(self.at)
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
            AppEvent::Batch(batch) => self.on_batch(batch),
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

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Resize(..) => self.cell = cell_size(),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.go(1),
            KeyCode::Char('k') | KeyCode::Up => self.go(-1),
            KeyCode::Char('r') => {
                self.failed_at = None;
                self.fetch_more();
            }
            _ => {}
        }
    }

    /// Shows the video `by` from this one, within the feed.
    fn go(&mut self, by: isize) {
        if self.videos.is_empty() {
            return;
        }
        self.at = self.at.saturating_add_signed(by).min(self.videos.len() - 1);
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
    use super::*;

    pub fn app_with(videos: Vec<Video>) -> App {
        let mut app = App::new(false, Vec::new(), PathBuf::new());
        app.cell = GUESSED_CELL;
        app.on_batch(Ok(Batch {
            videos,
            token: None,
            engine: Engine::Lightpanda,
            took: Duration::from_secs(17),
            note: None,
        }));
        app
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
}
