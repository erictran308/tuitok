//! The splash screen, with animations on, as tuitube's
//! (`../tuitube/src/ui/splash.rs`): the logo in the middle from the moment
//! tuitok has the terminal, while the rest starts behind it (how the
//! terminal draws pictures, the first videos and the first cover). `main`
//! draws it first, then `ui::draw` draws it over everything while it's up;
//! Pepper's picture isn't placed under it (`App::picture_in_box`). It leaves
//! once it's been up `HOLD` and the video shown's cover is in, or it's been
//! up `MOST`, fading out over `OUT`; the animations' fade at start then
//! brings in the screen (`effects`).

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tachyonfx::ColorSpace;
use unicode_width::UnicodeWidthStr;

use crate::theme::{self, Colors};

/// Up at least this long, so it's seen, however fast the rest is.
const HOLD: Duration = Duration::from_millis(700);
/// Up at most this long, whatever hasn't come.
const MOST: Duration = Duration::from_millis(1500);
/// How long it takes to fade out.
const OUT: Duration = Duration::from_millis(300);
/// How often it's drawn while up (the light across its words), and while
/// it fades out.
const UP_EVERY: Duration = Duration::from_millis(50);
const OUT_EVERY: Duration = Duration::from_millis(16);

const WORDS: &str = "t u i t o k";
const LOADING: &str = "Loading your videos…";

/// TikTok's two colors, along the note's edges as on its icon.
const CYAN: Color = Color::Rgb(0x25, 0xf4, 0xee);
const RED: Color = Color::Rgb(0xfe, 0x2c, 0x55);

/// The logo's sizes, in pixels two to a cell (half blocks), largest first:
/// a black box with round corners, a white note in it with a cyan edge up
/// and left of it and a red one down and right, as on TikTok's icon. The
/// largest that leaves room around it is drawn.
const LOGOS: [Logo; 2] = [
    Logo {
        side: 26,
        corner: 6.0,
    },
    Logo {
        side: 18,
        corner: 4.0,
    },
];

#[derive(Clone, Copy)]
struct Logo {
    side: u16,
    corner: f32,
}

/// The light across "Loading": once across, how far it reaches each side,
/// as the box's (`effects`).
const LIGHT_EVERY: f32 = 1.4;
const LIGHT_REACH: f32 = 6.0;

pub struct Splash {
    since: Instant,
    /// When it started fading out.
    leaving: Option<Instant>,
}

impl Splash {
    pub fn new(since: Instant) -> Self {
        Self {
            since,
            leaving: None,
        }
    }

    /// Moves on to `now`, given whether the first screen is all there (the
    /// video shown, and its cover): false once it's gone.
    pub fn advance(&mut self, now: Instant, ready: bool) -> bool {
        let up = now.saturating_duration_since(self.since);
        if self.leaving.is_none() && up >= HOLD && (ready || up >= MOST) {
            self.leaving = Some(now);
        }
        self.leaving
            .is_none_or(|leaving| now.saturating_duration_since(leaving) < OUT)
    }

    /// How long until the next frame.
    pub fn every(&self) -> Duration {
        if self.leaving.is_some() {
            OUT_EVERY
        } else {
            UP_EVERY
        }
    }

    /// How far it has faded out, 0 to 1.
    fn gone(&self, now: Instant) -> f32 {
        self.leaving
            .map_or(0.0, |leaving| {
                now.saturating_duration_since(leaving).as_secs_f32() / OUT.as_secs_f32()
            })
            .min(1.0)
    }
}

/// Draws the splash screen at once, in `theme`'s colors, as tuitok takes
/// the terminal (`main`, `--demo`); returns when, for `App::keep_splash`.
pub fn show(terminal: &mut ratatui::DefaultTerminal, theme: &str) -> Instant {
    let since = Instant::now();
    let colors = Colors::named(theme);
    let splash = Splash::new(since);
    // A terminal that can't be drawn to fails the first frame, as ever.
    let _ = terminal.draw(|f| {
        let area = f.area();
        draw(f.buffer_mut(), area, &colors, &splash, since);
    });
    since
}

/// Covers `area` with the splash as it is at `now`.
pub fn draw(buf: &mut Buffer, area: Rect, c: &Colors, splash: &Splash, now: Instant) {
    let gone = splash.gone(now);
    // Colors go to the background as it fades out.
    let faded = |color: Color| ColorSpace::Rgb.lerp(&color, &c.bg, gone);
    for p in area.positions() {
        buf[p].reset();
        buf[p].set_bg(c.bg).set_fg(faded(c.text));
    }
    // The logo, then a blank row, the words, a blank row, "Loading".
    let logo = LOGOS
        .into_iter()
        .find(|logo| area.width >= logo.side + 4 && area.height >= logo.side / 2 + 8);
    let logo_rows = logo.map_or(0, |logo| logo.side / 2);
    let height = logo_rows + if logo.is_some() { 4 } else { 3 };
    if area.height < height {
        return;
    }
    let top = area.y + (area.height - height) / 2;
    if let Some(logo) = logo {
        let left = area.x + (area.width - logo.side) / 2;
        for row in 0..logo_rows {
            for col in 0..logo.side {
                let upper = logo.pixel(col, 2 * row).map(faded);
                let lower = logo.pixel(col, 2 * row + 1).map(faded);
                let cell = &mut buf[(left + col, top + row)];
                match (upper, lower) {
                    (None, None) => {}
                    (upper, lower) if upper == lower => {
                        cell.set_bg(upper.unwrap_or(c.bg));
                    }
                    (upper, lower) => {
                        cell.set_symbol("▀")
                            .set_fg(upper.unwrap_or(c.bg))
                            .set_bg(lower.unwrap_or(c.bg));
                    }
                }
            }
        }
    }
    let words_row = top + height - 3;
    let style = Style::new().fg(faded(c.text)).add_modifier(Modifier::BOLD);
    centered(buf, area, words_row, WORDS, style);
    let at = centered(
        buf,
        area,
        words_row + 2,
        LOADING,
        Style::new().fg(faded(c.subtext)),
    );
    // A light across "Loading", as across what the box says while a video
    // comes.
    let width = LOADING.width() as f32;
    let pass = (now.saturating_duration_since(splash.since).as_secs_f32() / LIGHT_EVERY).fract();
    let center = f32::from(at.x) - LIGHT_REACH + pass * (width + 2.0 * LIGHT_REACH);
    for p in at.positions() {
        let near = 1.0 - (f32::from(p.x) - center).abs() / LIGHT_REACH;
        if near > 0.0 {
            let cell = &mut buf[p];
            cell.fg = ColorSpace::Rgb.lerp(&cell.fg, &faded(c.text), 0.85 * near * near);
        }
    }
}

/// Writes `text` in the middle of `row`, where it fits; returns where.
fn centered(buf: &mut Buffer, area: Rect, row: u16, text: &str, style: Style) -> Rect {
    let width = (text.width() as u16).min(area.width);
    let x = area.x + (area.width - width) / 2;
    buf.set_stringn(x, row, text, usize::from(width), style);
    Rect::new(x, row, width, 1)
}

impl Logo {
    /// Its pixel at `x`, `y`: the note's white, its cyan and red edges,
    /// the box's black, or none, outside the box.
    fn pixel(self, x: u16, y: u16) -> Option<Color> {
        let side = f32::from(self.side);
        let (x, y) = (f32::from(x) + 0.5, f32::from(y) + 0.5);
        // How far into a round corner, each way.
        let corner = self.corner;
        let dx = (corner - x).max(x - (side - corner)).max(0.0);
        let dy = (corner - y).max(y - (side - corner)).max(0.0);
        if dx.hypot(dy) > corner {
            return None;
        }
        // The edges are the note again, a pixel up and left, and down and
        // right, under it.
        let at = |dx: f32, dy: f32| note((x + dx) / side, (y + dy) / side);
        Some(if at(0.0, 0.0) {
            theme::WHITE
        } else if at(1.0, 1.0) {
            CYAN
        } else if at(-1.0, -1.0) {
            RED
        } else {
            theme::BLACK
        })
    }
}

/// Whether `(u, v)`, from 0 to 1 across the box and down it, is on the note
/// (♪): a round head low on the left, a stem up from its right, and a flag
/// curling right and down from the stem's top.
fn note(u: f32, v: f32) -> bool {
    let head = ((u - 0.40) / 0.14).hypot((v - 0.68) / 0.11) <= 1.0;
    let stem = (0.48..=0.56).contains(&u) && (0.22..=0.68).contains(&v);
    // A quarter of a ring round (0.56, 0.44), from the stem's top to its
    // right.
    let ring = (u - 0.56).hypot(v - 0.44);
    let flag = u >= 0.56 && v <= 0.44 && (0.12..=0.23).contains(&ring);
    head || stem || flag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        Colors::named(theme::DEFAULT)
    }

    fn text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_logo_and_words_sit_in_the_middle() {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let start = Instant::now();
        draw(&mut buf, area, &colors(), &Splash::new(start), start);
        let shown = text(&buf);
        assert!(shown.contains(WORDS) && shown.contains(LOADING), "{shown}");
        let has = |color: Color| {
            buf.content()
                .iter()
                .any(|cell| cell.bg == color || cell.fg == color)
        };
        assert!(has(theme::BLACK) && has(theme::WHITE) && has(CYAN) && has(RED));
        assert_eq!(buf[(0, 0)].bg, colors().bg);
        // As much room above the logo as under "Loading".
        let first = (0..24).find(|&y| (0..80).any(|x| buf[(x, y)].bg == theme::BLACK));
        let last = shown.lines().position(|l| l.contains(LOADING));
        let (above, under) = (first.unwrap(), 23 - last.unwrap() as u16);
        assert!(above.abs_diff(under) <= 1, "{above} above, {under} under");
    }

    #[test]
    fn a_small_window_gets_the_words_alone() {
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        let start = Instant::now();
        draw(&mut buf, area, &colors(), &Splash::new(start), start);
        assert!(text(&buf).contains(WORDS));
        assert!(!buf.content().iter().any(|cell| cell.bg == theme::BLACK));
    }

    #[test]
    fn it_leaves_once_up_long_enough_and_the_first_screen_is_there() {
        let start = Instant::now();
        let mut splash = Splash::new(start);
        assert!(splash.advance(start, true), "up at first");
        let held = start + HOLD / 2;
        assert!(splash.advance(held, true) && splash.leaving.is_none());
        let up = start + HOLD;
        assert!(splash.advance(up, false) && splash.leaving.is_none());
        assert!(splash.advance(up, true), "fading out");
        assert_eq!(splash.leaving, Some(up));
        assert_eq!(splash.every(), OUT_EVERY);
        assert!(!splash.advance(up + OUT, true), "gone");

        let mut slow = Splash::new(start);
        assert!(slow.advance(start + MOST, false));
        assert!(slow.leaving.is_some(), "what's still coming waits no more");
    }

    #[test]
    fn fading_out_it_goes_to_the_background() {
        let area = Rect::new(0, 0, 80, 24);
        let start = Instant::now();
        let mut splash = Splash::new(start);
        splash.leaving = Some(start);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &colors(), &splash, start + OUT);
        let c = colors();
        assert!(
            buf.content()
                .iter()
                .all(|cell| cell.bg == c.bg && cell.fg == c.bg)
        );
    }
}
