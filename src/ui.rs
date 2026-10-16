//! Drawing: the video's box, upright (9:16) and as tall as the window (the
//! whole tab with `f`), and beside it a panel with the video's author,
//! caption, sound, counts and where it's at, then the keys and the feed's
//! state on the bottom line. Until a video plays in Pepper's box, the box
//! shows its cover, with under it what plays and how it's going.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use crate::app::{App, FeedState};
use crate::cache::Download;
use crate::covers::Cover;
use crate::pepper;
use crate::video::{Video, VideoId};

/// The panel beside the video, in columns, shown in windows at least
/// `PANEL_FROM` columns wide.
const PANEL: u16 = 36;
const PANEL_FROM: u16 = 80;

/// Why videos play in mpv's window in Pepper: multiplexer `name` is between.
fn behind(name: &str) -> String {
    format!("Inside {name}, videos play in mpv's window: Pepper's box needs tuitok outside {name}.")
}

/// Rows of text under the cover in the box.
const TEXT_ROWS: u16 = 3;

/// Where things go in a window: the video's box, the panel beside it (in
/// wide windows), the bottom line. `App::box_pixels` lays it out the same.
pub struct Areas {
    pub video: Rect,
    pub panel: Option<Rect>,
    pub bottom: Rect,
}

/// Lays out a window of `area`: with `fullscreen` (`f`, in Pepper) the
/// video takes all but the bottom line, and there's no panel.
pub fn areas(area: Rect, cell: (u16, u16), fullscreen: bool) -> Areas {
    let [main, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if fullscreen {
        return Areas {
            video: main,
            panel: None,
            bottom,
        };
    }
    let (video_side, panel) = if main.width >= PANEL_FROM {
        let [video, panel] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(PANEL)]).areas(main);
        (video, Some(panel))
    } else {
        (main, None)
    };
    Areas {
        video: pepper::portrait_area(video_side, cell),
        panel,
        bottom,
    }
}

/// Where the cover goes in the video's box: all of it but `TEXT_ROWS` at
/// the bottom (all of it in the demo); nothing in a box too small for both.
pub fn cover_area(video: Rect, demo: bool) -> Rect {
    let text = if demo { 0 } else { TEXT_ROWS };
    if video.height < text + 4 {
        return Rect::default();
    }
    Rect::new(video.x, video.y, video.width, video.height - text)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let c = &app.colors;
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(c.bg).fg(c.text)), area);
    let areas = areas(area, app.cell, app.fullscreen);
    frame.render_widget(Block::new().style(Style::new().bg(c.panel)), areas.video);
    draw_in_box(frame, app, areas.video);
    if let Some(panel) = areas.panel {
        draw_panel(frame, app, panel);
    }
    draw_bottom(frame, app, areas.bottom);
}

/// In the box, centred: what would play and how its download is going, or
/// why there's nothing.
fn draw_in_box(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    // Pepper shows the picture over the box: black under it, nothing else.
    // True black: `Color::Black` is the palette's colour 0, which themes
    // paint grey (#45475a in Catppuccin).
    if app.picture_in_box() {
        frame.render_widget(
            Block::new().style(Style::new().bg(Color::Rgb(0, 0, 0))),
            area,
        );
        return;
    }
    let lines = match app.current() {
        Some(video) => {
            let pixels = (
                u32::from(area.width) * u32::from(app.cell.0),
                u32::from(area.height) * u32::from(app.cell.1),
            );
            let mut lines = vec![Line::from(clock(video.seconds)).fg(c.text)];
            if let Some(r) = video.best(pixels) {
                let mut what = format!("{} {}×{}", r.codec.name(), r.width, r.height);
                if let Some(bytes) = r.bytes {
                    what.push_str(&format!(" · {}", size(bytes)));
                }
                lines.push(Line::from(what).fg(c.dim));
            }
            lines.push(if let Some(e) = &app.play_error {
                Line::from(e.clone()).fg(c.red)
            } else if app.loading() {
                Line::from("Loading…").fg(c.dim)
            } else if app.shows_picture() {
                Line::from("Playing in mpv's window").fg(c.ok)
            } else if app.demo {
                Line::from("Made up: nothing plays").fg(c.dim)
            } else {
                download_line(app, &video.id)
            });
            lines
        }
        None => {
            let why = match app.feed {
                FeedState::Fetching(_) => "Fetching the feed…",
                _ => "No videos yet",
            };
            vec![Line::from(why).fg(c.dim)]
        }
    };
    let cover = app.current().and_then(|v| match app.covers.get(&v.id) {
        Some(Cover::Ready {
            encoded: Some(encoded),
            ..
        }) => Some(encoded),
        _ => None,
    });
    let picture = cover_area(area, app.demo);
    match cover {
        Some(encoded) if encoded.size == (picture.width, picture.height) && picture.height > 0 => {
            frame.render_widget(ratatui_image::Image::new(&encoded.protocol), picture);
            let below = Rect::new(
                area.x,
                picture.bottom(),
                area.width,
                area.bottom() - picture.bottom(),
            );
            frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), below);
        }
        _ => {
            let height = (lines.len() as u16).min(area.height);
            let middle = Rect::new(
                area.x,
                area.y + (area.height - height) / 2,
                area.width,
                height,
            );
            frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), middle);
        }
    }
}

/// How the download of video `id` is going.
fn download_line(app: &App, id: &VideoId) -> Line<'static> {
    let c = &app.colors;
    match app.downloads.get(id) {
        Some(Download::Getting { got, of: Some(of) }) if *of > 0 => {
            Line::from(format!("Downloading {}%", got * 100 / of)).fg(c.dim)
        }
        Some(Download::Getting { got, .. }) => {
            Line::from(format!("Downloading {}", size(*got))).fg(c.dim)
        }
        Some(Download::Ready(_)) => Line::from("Ready to play").fg(c.ok),
        Some(Download::Streams) => Line::from("Too big to download: streams").fg(c.dim),
        Some(Download::Failed(e)) => Line::from(format!("Download failed: {e}")).fg(c.red),
        None => Line::from("Waiting to download").fg(c.dim),
    }
}

fn draw_panel(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(c.border))
        .title(Line::from(" tuitok ").fg(c.accent).bold());
    let mut lines: Vec<Line> = Vec::new();
    match app.current() {
        Some(v) => {
            lines.push(Line::from(format!("@{}", v.author)).fg(c.accent).bold());
            if !v.name.is_empty() && v.name != v.author {
                lines.push(Line::from(v.name.clone()).fg(c.subtext));
            }
            if !v.caption.is_empty() {
                lines.push(Line::default());
                lines.extend(
                    v.caption
                        .lines()
                        .map(|line| Line::from(line.to_owned()).fg(c.text)),
                );
            }
            if !v.music.is_empty() {
                lines.push(Line::default());
                lines.push(Line::from(format!("♫ {}", v.music)).fg(c.subtext));
            }
            lines.push(Line::default());
            let counts = [
                (v.plays, "plays"),
                (v.likes, "likes"),
                (v.comments, "comments"),
                (v.shares, "shares"),
            ]
            .into_iter()
            .filter_map(|(n, what)| Some(format!("{} {what}", count(n?))))
            .collect::<Vec<_>>()
            .join(" · ");
            if !counts.is_empty() {
                lines.push(Line::from(counts).fg(c.dim));
            }
            let at = format!("{} of {}", app.at + 1, app.videos.len());
            match position(app, v) {
                Some((position, length, paused)) => {
                    let paused = if paused { " · paused" } else { "" };
                    let when = format!("{} / {}{paused} · {at}", clock(position), clock(length));
                    lines.push(Line::from(when).fg(c.dim));
                    lines.push(progress(
                        app,
                        position,
                        length,
                        area.width.saturating_sub(2),
                    ));
                }
                None => lines.push(Line::from(format!("{} · {at}", clock(v.seconds))).fg(c.dim)),
            }
            if let Some(name) = app.behind_multiplexer {
                lines.push(Line::default());
                lines.push(Line::from(behind(name)).fg(c.subtext));
            }
        }
        None if app.pepper => lines.push(Line::from("In Pepper: videos play in the box").fg(c.ok)),
        None => lines.push(match app.behind_multiplexer {
            Some(name) => Line::from(behind(name)).fg(c.subtext),
            None => Line::from("Not in Pepper: videos will play in mpv's window").fg(c.red),
        }),
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

/// Where the video shown is, how long it is, and whether it's paused,
/// while it plays (made up in the demo).
fn position(app: &App, video: &Video) -> Option<(u32, u32, bool)> {
    if app.demo {
        return Some((video.seconds * 2 / 5, video.seconds, false));
    }
    let length = app.duration.map_or(video.seconds, |d| d.round() as u32);
    app.shows_picture()
        .then_some((app.position as u32, length, app.paused))
}

/// A bar `width` cells wide, as far along as `position` is in `length`.
fn progress(app: &App, position: u32, length: u32, width: u16) -> Line<'static> {
    let c = &app.colors;
    let done = if length == 0 {
        0
    } else {
        (u64::from(width) * u64::from(position.min(length)) / u64::from(length)) as usize
    };
    Line::from(vec![
        Span::from("━".repeat(done)).fg(c.accent),
        Span::from("━".repeat(usize::from(width) - done)).fg(c.border),
    ])
}

/// The keys on the left, the feed's state on the right.
fn draw_bottom(frame: &mut Frame, app: &App, area: Rect) {
    let status = status(app);
    let width = (status.width() as u16).min(area.width / 2);
    let [keys, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(width + 1)]).areas(area);
    frame.render_widget(Paragraph::new(hints(app)), keys);
    frame.render_widget(Paragraph::new(status).alignment(Alignment::Right), right);
}

/// The keys, each in the accent colour, what it does dimmed.
fn hints(app: &App) -> Line<'static> {
    let c = &app.colors;
    let mut spans = Vec::new();
    for (keys, what) in [
        ("j k", "next/back"),
        ("space", "pause"),
        ("m", "mute"),
        ("f", "fill"),
        ("r", "more"),
        ("q", "quit"),
    ] {
        spans.push(Span::from(if spans.is_empty() { " " } else { "  " }));
        spans.push(Span::from(keys).fg(c.accent));
        spans.push(Span::from(format!(" {what}")).fg(c.dim));
    }
    Line::from(spans)
}

fn status(app: &App) -> Line<'static> {
    let c = &app.colors;
    match &app.feed {
        FeedState::NoBrowser => {
            Line::from("No browser for the feed: brew install lightpanda-io/browser/lightpanda")
                .fg(c.red)
        }
        FeedState::Fetching(engine) => {
            Line::from(format!("Fetching the feed with {}…", engine.name())).fg(c.dim)
        }
        FeedState::Fetched {
            new,
            engine,
            took,
            note,
        } => {
            let mut said = format!(
                "{new} new videos · {} · {:.0} s",
                engine.name(),
                took.as_secs_f64()
            );
            if let Some(note) = note {
                said.push_str(&format!(" (after: {note})"));
            }
            Line::from(said).fg(c.dim)
        }
        FeedState::Failed(e) => {
            Line::from(format!("The feed failed: {e} (r tries again)")).fg(c.red)
        }
    }
}

/// A length as minutes and seconds: `1:05`.
pub fn clock(seconds: u32) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// A count as TikTok shows it: `999`, `1.2K`, `12K`, `3.4M`.
pub fn count(n: u64) -> String {
    let short = |value: f64, unit: &str| {
        let digits = if value < 10.0 {
            format!("{value:.1}")
        } else {
            format!("{value:.0}")
        };
        format!("{}{unit}", digits.strip_suffix(".0").unwrap_or(&digits))
    };
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => short(n as f64 / 1e3, "K"),
        1_000_000..1_000_000_000 => short(n as f64 / 1e6, "M"),
        _ => short(n as f64 / 1e9, "B"),
    }
}

/// A file's size: `421 KB`, `1.5 MB`.
pub fn size(bytes: u64) -> String {
    if bytes < 1_000_000 {
        format!("{} KB", bytes.div_ceil(1_000))
    } else {
        format!("{:.1} MB", bytes as f64 / 1e6)
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::video::{Codec, Rendition, Video, VideoId};

    pub fn video(id: &str) -> Video {
        Video {
            id: VideoId::new(id).unwrap(),
            author: "someone".into(),
            name: "Some One".into(),
            caption: "first line\n\nsecond line".into(),
            music: "original sound · someone".into(),
            seconds: 75,
            plays: Some(1_234),
            likes: Some(56),
            comments: None,
            shares: Some(2_500_000),
            cover: None,
            renditions: vec![Rendition {
                codec: Codec::H265,
                width: 720,
                height: 1280,
                bytes: Some(779_704),
                urls: vec!["https://v16-webapp-prime.tiktok.com/x".into()],
            }],
            token: None,
        }
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn with_no_videos_the_box_says_why() {
        let mut app = crate::app::tests::app_with(Vec::new());
        app.feed = FeedState::NoBrowser;
        let text = screen(&app, 120, 40);
        assert!(text.contains("No videos yet"));
        assert!(text.contains(" tuitok "));
        assert!(text.contains("Not in Pepper"));
        let last = text.lines().last().unwrap();
        assert!(last.starts_with(" j k next/back  space pause  m mute  f fill  r more  q quit"));
        assert!(last.contains("No browser for the feed"));
    }

    #[test]
    fn a_video_shows_its_author_caption_counts_rendition_and_download() {
        let mut app = crate::app::tests::app_with(vec![video("1"), video("2")]);
        let text = screen(&app, 120, 40);
        for shown in [
            "@someone",
            "Some One",
            "first line",
            "second line",
            "♫ original sound",
            "1:15 · 1 of 2",
            "H.265 720×1280 · 780 KB",
            "Waiting to download",
            "2 new videos · Lightpanda · 17 s",
        ] {
            assert!(text.contains(shown), "{shown}");
        }
        // The counts wrap at the panel's width.
        for shown in ["1.2K plays", "56 likes", "2.5M", "shares"] {
            assert!(text.contains(shown), "{shown}");
        }
        let id = VideoId::new("1").unwrap();
        for (state, shown) in [
            (
                Download::Getting {
                    got: 390_000,
                    of: Some(779_704),
                },
                "Downloading 50%",
            ),
            (
                Download::Getting {
                    got: 390_000,
                    of: None,
                },
                "Downloading 390 KB",
            ),
            (Download::Ready(PathBuf::from("/x.mp4")), "Ready to play"),
            (Download::Streams, "Too big to download: streams"),
            (
                Download::Failed("HTTP 403".into()),
                "Download failed: HTTP 403",
            ),
        ] {
            app.downloads.insert(id.clone(), state);
            assert!(screen(&app, 120, 40).contains(shown), "{shown}");
        }
    }

    #[test]
    fn playing() {
        let mut app = crate::app::tests::app_with(vec![video("1"), video("2")]);
        // In mpv's window: the box says so, the panel says where it is.
        crate::app::tests::set_playing(&mut app, "1", false);
        app.position = 3.4;
        app.paused = true;
        let text = screen(&app, 120, 40);
        assert!(text.contains("Playing in mpv's window"));
        assert!(text.contains("0:03 / 1:15 · paused · 1 of 2"));
        // In Pepper's box: nothing drawn in the box, where the picture goes.
        crate::app::tests::set_playing(&mut app, "1", true);
        let text = screen(&app, 120, 40);
        assert!(!text.contains("Playing in mpv's window") && !text.contains("H.265"));
        assert!(text.contains("0:03 / 1:15 · paused · 1 of 2"));
        // Why a video doesn't play.
        app.playing = None;
        app.play_error = Some("It can't play: loading failed".into());
        assert!(screen(&app, 120, 40).contains("It can't play: loading failed"));
    }

    #[test]
    fn a_cover_shows_in_the_box_with_the_text_under_it() {
        use crate::covers::{Cover, encode_now};
        let mut app = crate::app::tests::app_with(vec![video("1")]);
        let video_box = areas(Rect::new(0, 0, 120, 40), app.cell, false).video;
        let picture = cover_area(video_box, false);
        let encoded = encode_now(
            &ratatui_image::picker::Picker::halfblocks(),
            &crate::demo::cover(0),
            (picture.width, picture.height),
        )
        .unwrap();
        let cover = Cover::Ready {
            picture: std::sync::Arc::new(crate::demo::cover(0)),
            encoded: Some(encoded),
            encoding: false,
        };
        app.covers.insert(VideoId::new("1").unwrap(), cover);
        let text = screen(&app, 120, 40);
        let rows: Vec<&str> = text.lines().collect();
        // Half blocks in the box, the text in its bottom rows.
        let middle = rows[usize::from(video_box.y + video_box.height / 2)];
        assert!(middle.contains('▀') || middle.contains('▄'), "{middle}");
        let under = rows[usize::from(picture.bottom())..usize::from(video_box.bottom())].join("\n");
        assert!(
            under.contains("1:15") && under.contains("Waiting to download"),
            "{under}"
        );
    }

    #[test]
    fn the_demo_and_filling_the_tab() {
        let mut app = crate::app::App::demo(true);
        app.cell = (9, 18);
        app.size = (120, 40);
        let text = screen(&app, 120, 40);
        assert!(text.contains("@made.up.cat") && text.contains("1 of 6"));
        assert!(text.contains("0:05 / 0:14 · 1 of 6") && text.contains("━"));
        assert!(!text.contains("Waiting to download"));
        // f: the box takes all but the bottom line, and the panel goes.
        let filled = areas(Rect::new(0, 0, 120, 40), app.cell, true);
        assert_eq!(
            (filled.video, filled.panel),
            (Rect::new(0, 0, 120, 39), None)
        );
        app.fullscreen = true;
        assert!(!screen(&app, 120, 40).contains("@made.up.cat"));
    }

    #[test]
    fn inside_a_multiplexer_in_pepper_the_panel_says_why_videos_play_in_a_window() {
        let mut app = crate::app::tests::app_with(vec![video("1")]);
        app.behind_multiplexer = Some("herdr");
        let text = screen(&app, 120, 40);
        // Wrapped at the panel's width.
        assert!(
            text.contains("Inside herdr,") && text.contains("outside herdr."),
            "{text}"
        );
        app.videos.clear();
        assert!(screen(&app, 120, 40).contains("Inside herdr,"));
    }

    #[test]
    fn a_narrow_window_has_no_panel() {
        let mut app = crate::app::tests::app_with(Vec::new());
        app.pepper = true;
        let text = screen(&app, 60, 30);
        assert!(text.contains("No videos yet"));
        assert!(!text.contains(" tuitok "));
    }

    #[test]
    fn numbers() {
        assert_eq!(clock(75), "1:15");
        assert_eq!(clock(5), "0:05");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_234), "1.2K");
        assert_eq!(count(12_345), "12K");
        assert_eq!(count(1_000), "1K");
        assert_eq!(count(3_400_000), "3.4M");
        assert_eq!(size(431_361), "432 KB");
        assert_eq!(size(1_481_543), "1.5 MB");
    }
}
