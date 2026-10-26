//! Animations (`animations` in settings, on by default), drawn with
//! tachyonfx over each frame, as tuitube's (`../tuitube/src/ui/effects.rs`,
//! cut down to what tuitok shows): the screen comes out of the background
//! at start, the panel sweeps in for each new video, and a light runs
//! across what the video's box says while it waits (loading, downloading,
//! fetching the feed, opening a link) (`under`, before the boxes over the
//! bottom line and the `?` popup are drawn); the `?` popup opens out from
//! its middle while the screen behind it dims (`over`, after it). `/`'s and
//! `:`'s boxes dim nothing, as vim's completions.
//!
//! Only colors change, never characters (a wide letter would shift a
//! column), and never over a picture the terminal draws: a cover in kitty's
//! protocol is placeholder characters whose color is the picture's id,
//! sixel's and iTerm2's are drawn once over cells the frame then skips, and
//! Pepper's video box is Pepper's. Those cells are put back as they were
//! drawn once the animations ran. Covers in half blocks are colored cells,
//! and fade and dim with the rest. Under the popup Pepper's box dims too:
//! what shows in its holes dims with the rest.
//!
//! Frames come only while something moves (`Effects::moving`, `App::run`):
//! every `EVERY`, or `SHIMMER_EVERY` while only the light does, as that
//! lasts as long as the video takes to come. The dimming then stays as it
//! ended, drawn with each frame, until the popup closes. Never in tests
//! (`Effects.live`), whose frames mustn't change with the time they took
//! to draw.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::pattern::DiagonalPattern;
use tachyonfx::{ColorSpace, Effect, Interpolation, Motion, fx};

use crate::app::App;
use crate::video::VideoId;

/// What the setting turns on, for the settings tab.
pub const ABOUT: &str = "Animations: the screen fades in, the panel sweeps in, popups open out";

/// How often a frame is drawn while something moves, and while only the
/// light across what the box says does.
const EVERY: Duration = Duration::from_millis(16);
const SHIMMER_EVERY: Duration = Duration::from_millis(50);
/// At most this often over ssh (`Effects.remote`): every color of the
/// screen changes as the popup opens.
const REMOTE_EVERY: Duration = Duration::from_millis(33);

/// How long each takes, in milliseconds.
const INTRO_MS: u32 = 900;
const POPUP_MS: u32 = 260;
const DIM_MS: u32 = 200;
const PANEL_MS: u32 = 500;
/// Once across what the box says, left to right.
const SHIMMER_MS: u32 = 1400;
/// How far the screen behind the popup goes toward the background.
const DIM: f32 = 0.6;
/// How soft the edge of the popup opening is, in columns.
const POPUP_EDGE: f32 = 6.0;
/// The light across what the box says: how far it reaches each side, in
/// columns, and how near the text's color gets to the brightest.
const SHIMMER_REACH: f32 = 8.0;
const SHIMMER_LIGHT: f32 = 0.85;

/// What an animation is for: a new one takes the place of the one of its
/// kind. In the order they're drawn: under the popup up to `Dim`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Intro,
    Loading,
    Panel,
    /// The screen behind the popup, dimming, or coming back once it closed.
    Dim,
    /// The popup opening.
    Popup,
}

struct Running {
    kind: Kind,
    fx: Effect,
    /// Not drawn yet: it starts from its beginning, whenever the frame
    /// before was.
    fresh: bool,
    /// The dimming's: how far it starts and how far it goes.
    dims: Option<(f32, f32)>,
}

impl Running {
    /// Drawn as it ended until it's replaced: the dimming behind the popup,
    /// not the brightening once it closed.
    fn held(&self) -> bool {
        self.dims.is_some_and(|(_, to)| to > 0.0)
    }
}

/// The theme's colors the animations blend with, as they are in this
/// frame: a theme picked behind the popup dims toward its own background.
#[derive(Clone, Copy, Default)]
struct Palette {
    bg: Color,
    panel: Color,
    text: Color,
}

/// What a frame shows under the popup that animations start from.
#[derive(Clone, Debug, Default)]
pub struct Shown {
    /// The panel's inside, where it's drawn.
    pub panel: Option<Rect>,
    /// The video shown.
    pub video: Option<VideoId>,
    /// The row of the video's box that says it waits (`ui::draw_in_box`).
    pub waiting: Option<Rect>,
}

#[derive(Default)]
pub struct Effects {
    /// Drawn to a terminal (`for_terminal`), never in tests.
    pub live: bool,
    /// Over ssh: fewer frames, `REMOTE_EVERY`, and no fade at start.
    pub remote: bool,
    running: Vec<Running>,
    palette: Rc<Cell<Palette>>,
    /// What the frame before showed under the popup; `None` before the
    /// first.
    seen: Option<Shown>,
    /// The popup was open in the frame before.
    seen_popup: bool,
    /// When the frame before was drawn, and how long it's been since, while
    /// something moved (an idle wait isn't skipped through).
    last: Option<Instant>,
    elapsed: Duration,
}

impl Effects {
    /// For drawing to a terminal (`main`, `--demo`): the animations run,
    /// with fewer frames over ssh.
    pub fn for_terminal() -> Self {
        Self {
            live: true,
            remote: ["SSH_CONNECTION", "SSH_TTY"]
                .iter()
                .any(|name| std::env::var_os(name).is_some()),
            ..Self::default()
        }
    }

    /// Something moves: the next frame is drawn in `every`. The dimming
    /// held behind the popup doesn't move.
    pub fn moving(&self) -> bool {
        self.running.iter().any(|r| r.fx.running())
    }

    /// How long until the next frame, while something moves.
    pub fn every(&self) -> Duration {
        let busy = self
            .running
            .iter()
            .any(|r| r.kind != Kind::Loading && r.fx.running());
        let every = if busy { EVERY } else { SHIMMER_EVERY };
        if self.remote {
            every.max(REMOTE_EVERY)
        } else {
            every
        }
    }

    /// Starts what `shown` shows anew, after `seen`, what the frame before
    /// showed.
    fn start_under(&mut self, seen: Option<&Shown>, shown: &Shown) {
        let bg = self.palette.get().bg;
        let Some(seen) = seen else {
            // The first frame: all of it comes out of the background. Not
            // over ssh, where the first screen would come later for it.
            if !self.remote {
                self.start(Kind::Intro, intro(bg));
            }
            return;
        };
        // The panel coming up, and each new video shown in it; the sweep
        // follows it as the window changes.
        let new_video = shown.video.is_some() && shown.video != seen.video;
        let room = |panel: &Rect| !panel.is_empty();
        match shown.panel.filter(room) {
            Some(panel) if seen.panel.filter(room).is_none() || new_video => {
                self.start(Kind::Panel, sweep(bg).with_area(panel));
            }
            Some(panel) => self.move_to(Kind::Panel, panel),
            None => self.stop(Kind::Panel),
        }
        self.light(shown.waiting);
    }

    /// Lights `place`, the row saying the box waits, for as long as it
    /// shows: not under the popup, which would dim it and redraw the screen
    /// for it 20 times a second to no end; again once animations are turned
    /// back on.
    fn light(&mut self, place: Option<Rect>) {
        let lit = place.filter(|place| !place.is_empty());
        match lit.filter(|_| !self.seen_popup) {
            Some(place) if self.running.iter().any(|r| r.kind == Kind::Loading) => {
                self.move_to(Kind::Loading, place);
            }
            Some(place) => {
                let fx = shimmer(self.palette.clone()).with_area(place);
                self.start(Kind::Loading, fx);
            }
            None => self.stop(Kind::Loading),
        }
    }

    /// Dims the screen while the `popup` is open, and opens it out if it
    /// wasn't open in the frame before.
    fn start_over(&mut self, popup: Option<Rect>) {
        let goal = if popup.is_some() { DIM } else { 0.0 };
        let (now, going) = self.dimming();
        if going != goal && (goal > 0.0 || now > 0.0) {
            // From where it is: a popup closed before the screen was all
            // dimmed brightens it from there.
            let dimming = dim(self.palette.clone(), now, goal);
            self.start(Kind::Dim, dimming).dims = Some((now, goal));
        }
        match popup {
            Some(place) if !self.seen_popup && !place.is_empty() => {
                let opening = opening(self.palette.clone()).with_area(place);
                self.start(Kind::Popup, opening);
            }
            Some(place) => self.move_to(Kind::Popup, place),
            None => self.stop(Kind::Popup),
        }
    }

    /// How far the screen is dimmed now, and how far it's going.
    fn dimming(&self) -> (f32, f32) {
        let Some((r, (from, to))) = self
            .running
            .iter()
            .find_map(|r| r.dims.map(|dims| (r, dims)))
        else {
            return (0.0, 0.0);
        };
        let alpha = r.fx.timer().map_or(1.0, |t| t.alpha());
        (from + (to - from) * alpha, to)
    }

    fn start(&mut self, kind: Kind, fx: Effect) -> &mut Running {
        self.stop(kind);
        let at = self.running.partition_point(|r| r.kind < kind);
        let fx = Running {
            kind,
            fx,
            fresh: true,
            dims: None,
        };
        self.running.insert(at, fx);
        &mut self.running[at]
    }

    fn stop(&mut self, kind: Kind) {
        self.running.retain(|r| r.kind != kind);
    }

    /// What `kind` animates moved, or changed size.
    fn move_to(&mut self, kind: Kind, area: Rect) {
        if let Some(r) = self.running.iter_mut().find(|r| r.kind == kind) {
            r.fx.set_area(area);
        }
    }

    /// The time since the frame before, if something moved since then.
    fn tick(&mut self, now: Instant) {
        self.elapsed = match self.last {
            Some(last) if self.moving() => now.saturating_duration_since(last),
            _ => Duration::ZERO,
        };
        self.last = Some(now);
    }

    /// Draws the animations of the kinds `here` takes, then puts back the
    /// cells in `keep` as they were. Each first sees the colors its cells
    /// show (`settle`).
    fn draw(&mut self, here: impl Fn(Kind) -> bool, buf: &mut Buffer, keep: &[Rect]) {
        if !self.running.iter().any(|r| here(r.kind)) {
            return;
        }
        let screen = buf.area;
        // Only what an animation here would touch is kept.
        let touched: Vec<Rect> = self
            .running
            .iter()
            .filter(|r| here(r.kind))
            .map(|r| r.fx.area().unwrap_or(screen))
            .collect();
        // Only colors are animated, so only colors are kept (a kitty
        // picture's first cell can hold the whole picture, as text).
        let kept: Vec<(Rect, Vec<(Color, Color)>)> = keep
            .iter()
            .map(|r| r.intersection(screen))
            .filter(|r| touched.iter().any(|t| t.intersects(*r)))
            .map(|r| (r, r.positions().map(|p| (buf[p].fg, buf[p].bg)).collect()))
            .collect();
        let palette = self.palette.get();
        for r in self.running.iter_mut().filter(|r| here(r.kind)) {
            // Within the window, should it have shrunk.
            if let Some(area) = r.fx.area() {
                r.fx.set_area(area.intersection(screen));
            }
            settle(buf, r.fx.area().unwrap_or(screen), palette);
            let elapsed = if r.fresh {
                Duration::ZERO
            } else {
                self.elapsed
            };
            r.fresh = false;
            r.fx.process(elapsed, buf, screen);
        }
        self.running
            .retain(|r| !here(r.kind) || r.held() || r.fx.running());
        for (area, colors) in kept {
            for (p, (fg, bg)) in area.positions().zip(colors) {
                buf[p].fg = fg;
                buf[p].bg = bg;
            }
        }
    }
}

/// Before the boxes over the bottom line and the popup are drawn: starts
/// what this frame shows anew, then draws the animations under the popup,
/// leaving `pictures` be (the cover the terminal draws, Pepper's box).
pub fn under(app: &App, buf: &mut Buffer, shown: Shown, pictures: &[Rect], now: Instant) {
    let mut effects = app.effects.borrow_mut();
    let on = effects.live && app.animations;
    let c = &app.colors;
    effects.palette.set(Palette {
        bg: c.bg,
        panel: c.panel,
        text: c.text,
    });
    effects.tick(now);
    let seen = effects.seen.take();
    if on {
        effects.start_under(seen.as_ref(), &shown);
    } else {
        effects.running.clear();
    }
    effects.seen = Some(shown);
    effects.draw(|kind| kind < Kind::Dim, buf, pictures);
}

/// After the popup is drawn (`popup`, where, while it's open): the screen
/// behind it dims, leaving `pictures` (the cover the terminal draws) and
/// the popup be, and it opens out if it just opened. Pepper's box (in
/// `pictures` only with no popup) dims with the rest under it: its picture
/// plays on around the popup where Pepper takes holes, and what shows in
/// them dims too.
pub fn over(app: &App, buf: &mut Buffer, popup: Option<Rect>, pictures: &[Rect]) {
    let mut effects = app.effects.borrow_mut();
    let on = effects.live && app.animations;
    if on {
        effects.start_over(popup);
    }
    effects.seen_popup = popup.is_some();
    let mut keep = pictures.to_vec();
    keep.extend(popup);
    effects.draw(|kind| kind == Kind::Dim, buf, &keep);
    effects.draw(|kind| kind == Kind::Popup, buf, &[]);
}

/// Gives the cells in `area` left to the terminal (`Reset`, as `Clear`
/// leaves them) the theme's colors before an animation blends them:
/// tachyonfx would take them for black. Every other color tuitok draws is
/// exact (`theme::WHITE`, `theme::BLACK`), so none is the palette's guess.
fn settle(buf: &mut Buffer, area: Rect, palette: Palette) {
    for p in area.intersection(buf.area).positions() {
        let cell = &mut buf[p];
        if cell.fg == Color::Reset {
            cell.fg = palette.text;
        }
        if cell.bg == Color::Reset {
            cell.bg = palette.bg;
        }
    }
}

/// The whole screen comes out of the background, from its top left corner
/// (a fade from a color runs backwards, so its pattern starts at the other).
fn intro(bg: Color) -> Effect {
    fx::fade_from(bg, bg, (INTRO_MS, Interpolation::QuadOut))
        .with_pattern(DiagonalPattern::bottom_right_to_top_left().with_transition_width(12.0))
        .with_color_space(ColorSpace::Rgb)
}

/// The popup comes out of its own background from its middle out, in an
/// oval as wide as its box, its edge soft (`POPUP_EDGE` columns).
fn opening(palette: Rc<Cell<Palette>>) -> Effect {
    fx::effect_fn(
        (),
        (POPUP_MS, Interpolation::QuadOut),
        move |_, context, cells| {
            let panel = palette.get().panel;
            let area = context.area;
            let middle_x = f32::from(area.x) + f32::from(area.width) / 2.0;
            let middle_y = f32::from(area.y) + f32::from(area.height) / 2.0;
            // A row is about two columns tall.
            let far = (f32::from(area.width) / 2.0).hypot(f32::from(area.height));
            let edge = context.alpha() * (far + POPUP_EDGE);
            for (p, cell) in cells {
                let x = f32::from(p.x) + 0.5 - middle_x;
                let y = 2.0 * (f32::from(p.y) + 0.5 - middle_y);
                let hidden = 1.0 - ((edge - x.hypot(y)) / POPUP_EDGE).clamp(0.0, 1.0);
                if hidden > 0.0 {
                    cell.fg = ColorSpace::Rgb.lerp(&cell.fg, &panel, hidden);
                    cell.bg = ColorSpace::Rgb.lerp(&cell.bg, &panel, hidden);
                }
            }
        },
    )
}

/// Every color behind the popup goes from `from` to `to` of the way to the
/// background, so it stands out on any theme.
fn dim(palette: Rc<Cell<Palette>>, from: f32, to: f32) -> Effect {
    fx::effect_fn(
        (),
        (DIM_MS, Interpolation::QuadOut),
        move |_, context, cells| {
            let bg = palette.get().bg;
            let amount = from + (to - from) * context.alpha();
            for (_, cell) in cells {
                cell.fg = ColorSpace::Rgb.lerp(&cell.fg, &bg, amount);
                cell.bg = ColorSpace::Rgb.lerp(&cell.bg, &bg, amount);
            }
        },
    )
}

/// The panel sweeps in from the left, as tuitube's player bar does.
fn sweep(bg: Color) -> Effect {
    fx::sweep_in(
        Motion::LeftToRight,
        30,
        0,
        bg,
        (PANEL_MS, Interpolation::QuadOut),
    )
    // Straight from one color to the other, as every animation here: the
    // default goes round the color wheel (red through purple to blue).
    .with_color_space(ColorSpace::Rgb)
}

/// A light runs across the row, left to right, once every `SHIMMER_MS`,
/// over and over, reaching `SHIMMER_REACH` columns each side: each cell's
/// text as bright as the theme's text while it passes.
fn shimmer(palette: Rc<Cell<Palette>>) -> Effect {
    let pass = fx::effect_fn(
        (),
        (SHIMMER_MS, Interpolation::SineInOut),
        move |_, context, cells| {
            let light = palette.get().text;
            let area = context.area;
            // From out of sight on the left to out of sight on the right.
            let across = f32::from(area.width) + 2.0 * SHIMMER_REACH;
            let at = f32::from(area.x) - SHIMMER_REACH + context.alpha() * across;
            for (p, cell) in cells {
                let near = 1.0 - (f32::from(p.x) - at).abs() / SHIMMER_REACH;
                if near > 0.0 {
                    let amount = SHIMMER_LIGHT * near * near;
                    cell.fg = ColorSpace::Rgb.lerp(&cell.fg, &light, amount);
                }
            }
        },
    );
    fx::repeating(pass)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::app_with;

    /// A screen of text in the theme's colors, with a kitty picture's
    /// placeholders in `picture`, their color its id.
    fn drawn(app: &App, picture: Rect) -> Buffer {
        let c = &app.colors;
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        for p in buf.area.positions() {
            buf[p].set_symbol("x").set_fg(c.subtext).set_bg(c.panel);
        }
        for p in picture.positions() {
            buf[p].set_symbol("\u{10EEEE}").set_fg(Color::Rgb(0, 0, 7));
        }
        buf
    }

    /// One frame at `at`, as `ui::draw` draws it: the animations under the
    /// popup, then the `popup` drawn as it is, then the animations over it.
    fn frame(app: &App, shown: Shown, popup: Option<Rect>, at: Instant, picture: Rect) -> Buffer {
        let mut buf = drawn(app, picture);
        let as_drawn = buf.clone();
        under(app, &mut buf, shown, &[picture], at);
        if let Some(place) = popup {
            for p in place.positions() {
                buf[p] = as_drawn[p].clone();
            }
        }
        over(app, &mut buf, popup, &[picture]);
        buf
    }

    /// Drawn to a terminal.
    fn live() -> App {
        let app = app_with(Vec::new());
        app.effects.borrow_mut().live = true;
        app
    }

    /// Live, and its first frame's animation over by `start + LATER`.
    fn settled(start: Instant) -> App {
        let app = live();
        frame(&app, Shown::default(), None, start, PICTURE);
        frame(&app, Shown::default(), None, start + LATER, PICTURE);
        assert!(!app.effects.borrow().moving());
        app
    }

    fn dimmed(app: &App) -> Color {
        ColorSpace::Rgb.lerp(&app.colors.subtext, &app.colors.bg, DIM)
    }

    fn moving(app: &App) -> bool {
        app.effects.borrow().moving()
    }

    const PICTURE: Rect = Rect::new(2, 6, 6, 3);
    const POPUP: Option<Rect> = Some(Rect::new(10, 2, 20, 6));
    const LATER: Duration = Duration::from_secs(2);
    const SOON: Duration = Duration::from_millis(50);

    #[test]
    fn the_screen_comes_out_of_the_background_but_pictures_stay() {
        let app = live();
        let start = Instant::now();
        let first = frame(&app, Shown::default(), None, start, PICTURE);
        assert_eq!(first[(39, 0)].fg, app.colors.bg, "hidden at first");
        assert!(moving(&app));
        let picture = drawn(&app, PICTURE);
        for p in PICTURE.positions() {
            assert_eq!(first[p], picture[p], "a picture's cells as drawn");
        }
        let end = frame(&app, Shown::default(), None, start + EVERY, PICTURE);
        assert_eq!(end[(39, 9)].fg, app.colors.bg, "the far corner comes last");
        let end = frame(&app, Shown::default(), None, start + LATER, PICTURE);
        assert_eq!(end, drawn(&app, PICTURE), "all as drawn once it's over");
        assert!(!moving(&app), "no more frames");
    }

    #[test]
    fn the_popup_dims_the_screen_behind_it_until_it_closes() {
        let start = Instant::now();
        let app = settled(start);
        let t = start + LATER;
        let opening = frame(&app, Shown::default(), POPUP, t, PICTURE);
        assert!(moving(&app));
        assert_eq!(opening[(20, 5)].fg, app.colors.panel, "it opens out");
        let shown = frame(&app, Shown::default(), POPUP, t + LATER, PICTURE);
        let c = &app.colors;
        assert_eq!(shown[(0, 0)].fg, dimmed(&app), "dimmed behind it");
        assert_eq!(
            shown[(39, 9)].bg,
            ColorSpace::Rgb.lerp(&c.panel, &c.bg, DIM)
        );
        assert_eq!(shown[(12, 3)].fg, c.subtext, "the popup as drawn");
        assert_eq!(shown[(2, 6)], drawn(&app, PICTURE)[(2, 6)], "and pictures");
        assert!(!moving(&app), "held as it ended, with no frames");

        let closed = frame(&app, Shown::default(), None, t + LATER, PICTURE);
        assert_eq!(
            closed[(0, 0)].fg,
            dimmed(&app),
            "comes back from where it was"
        );
        assert!(moving(&app));
        let closed = frame(&app, Shown::default(), None, t + 2 * LATER, PICTURE);
        assert_eq!(closed, drawn(&app, PICTURE));
        assert!(!moving(&app));
    }

    #[test]
    fn a_popup_closed_while_it_dims_brightens_it_from_where_it_got() {
        let start = Instant::now();
        let app = settled(start);
        let t = start + LATER;
        frame(&app, Shown::default(), POPUP, t, PICTURE);
        let part = frame(&app, Shown::default(), POPUP, t + SOON, PICTURE)[(0, 0)].fg;
        assert_ne!(part, app.colors.subtext);
        assert_ne!(part, dimmed(&app), "not all the way yet");
        let closed = frame(&app, Shown::default(), None, t + SOON, PICTURE);
        assert_eq!(closed[(0, 0)].fg, part, "no darker than it got");
    }

    #[test]
    fn a_light_runs_across_what_the_box_says_until_the_video_plays() {
        let start = Instant::now();
        let app = settled(start);
        let t = start + LATER;
        let row = Rect::new(1, 8, 38, 1);
        let waiting = Shown {
            waiting: Some(row),
            ..Shown::default()
        };
        frame(&app, waiting.clone(), None, t, Rect::default());
        assert_eq!(
            app.effects.borrow().every(),
            SHIMMER_EVERY,
            "few frames for it"
        );
        // Halfway through a pass, the light is in the middle of the row.
        let at = t + Duration::from_millis(u64::from(SHIMMER_MS / 2));
        let shown = frame(&app, waiting.clone(), None, at, Rect::default());
        let subtext = app.colors.subtext;
        assert_ne!(shown[(20, 8)].fg, subtext, "lit in the middle");
        assert_eq!(shown[(2, 8)].fg, subtext, "and only there");
        // It goes on for as long as it waits; not under the popup.
        frame(
            &app,
            waiting.clone(),
            None,
            at + 10 * LATER,
            Rect::default(),
        );
        assert!(moving(&app));
        frame(
            &app,
            waiting.clone(),
            POPUP,
            at + 11 * LATER,
            Rect::default(),
        );
        frame(&app, waiting, POPUP, at + 12 * LATER, Rect::default());
        assert!(!moving(&app), "nothing lit under the popup");
        frame(
            &app,
            Shown::default(),
            None,
            at + 13 * LATER,
            Rect::default(),
        );
        frame(
            &app,
            Shown::default(),
            None,
            at + 14 * LATER,
            Rect::default(),
        );
        assert!(!moving(&app), "no light once it plays");
    }

    #[test]
    fn the_panel_sweeps_in_for_each_new_video() {
        let start = Instant::now();
        let app = settled(start);
        let t = start + LATER;
        let panel = Rect::new(20, 1, 19, 8);
        let showing = |id: &str| Shown {
            panel: Some(panel),
            video: Some(VideoId::new(id).unwrap()),
            ..Shown::default()
        };
        let swept = frame(&app, showing("1"), None, t, Rect::default());
        assert_eq!(swept[(38, 4)].fg, app.colors.bg, "it sweeps in");
        frame(&app, showing("1"), None, t + LATER, Rect::default());
        assert!(!moving(&app), "the same video sweeps in once");
        let next = frame(&app, showing("2"), None, t + LATER, Rect::default());
        assert_eq!(next[(38, 4)].fg, app.colors.bg, "the next one does too");
        assert_eq!(next[(5, 4)].fg, app.colors.subtext, "only the panel");
    }

    #[test]
    fn nothing_moves_in_tests_or_with_animations_off() {
        let app = app_with(Vec::new());
        let start = Instant::now();
        let first = frame(&app, Shown::default(), POPUP, start, PICTURE);
        assert_eq!(first, drawn(&app, PICTURE), "tests draw what's there");
        assert!(!moving(&app));

        let mut app = live();
        app.animations = false;
        let first = frame(&app, Shown::default(), POPUP, start, PICTURE);
        assert_eq!(first, drawn(&app, PICTURE));
        assert!(!moving(&app));

        let mut app = live();
        frame(&app, Shown::default(), POPUP, start, PICTURE);
        assert!(moving(&app));
        app.animations = false;
        let now = frame(&app, Shown::default(), POPUP, start + EVERY, PICTURE);
        assert_eq!(now, drawn(&app, PICTURE), "turned off, it stops at once");
    }

    #[test]
    fn the_ui_draws_only_exact_colors_so_none_snaps_as_it_blends() {
        use crate::app::{CommandLine, HelpMenu, HelpTab, Search};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let exact = |color: Color| matches!(color, Color::Rgb(..) | Color::Reset);
        let mut app = app_with(vec![crate::ui::tests::video("1")]);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut check = |app: &App| {
            terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
            for cell in terminal.backend().buffer().content() {
                assert!(exact(cell.fg) && exact(cell.bg), "{cell:?}");
            }
        };
        check(&app);
        app.help = Some(HelpMenu {
            tab: HelpTab::Settings,
            scroll: Default::default(),
            selected: 0,
            error: None,
        });
        check(&app);
        app.help = None;
        app.search = Some(Search::default());
        check(&app);
        app.search = None;
        app.command = Some(CommandLine::default());
        check(&app);
        app.command = None;
        crate::app::tests::set_playing(&mut app, "1", true);
        (app.pepper, app.pepper_holes, app.fullscreen) = (true, true, true);
        app.paused = true;
        check(&app);
        // The splash screen.
        app.keep_splash(Instant::now());
        check(&app);
    }
}
