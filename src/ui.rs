//! Drawing: the video's box, upright (9:16) and as tall as the window, and
//! beside it a panel with the video's author, caption, sound and counts,
//! then the keys and the feed's state on the bottom line. Until playing
//! lands, the box says what would play: the length and the rendition.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use crate::app::{App, FeedState};
use crate::pepper;

/// The panel beside the video, in columns, shown in windows at least
/// `PANEL_FROM` columns wide.
const PANEL: u16 = 36;
const PANEL_FROM: u16 = 80;

pub fn draw(frame: &mut Frame, app: &App) {
    let c = &app.colors;
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(c.bg).fg(c.text)), area);
    let [main, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let (video_side, panel) = if main.width >= PANEL_FROM {
        let [video, panel] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(PANEL)]).areas(main);
        (video, Some(panel))
    } else {
        (main, None)
    };
    let video = pepper::portrait_area(video_side, app.cell);
    frame.render_widget(Block::new().style(Style::new().bg(c.panel)), video);
    draw_in_box(frame, app, video);
    if let Some(panel) = panel {
        draw_panel(frame, app, panel);
    }
    draw_bottom(frame, app, bottom);
}

/// In the box, centred: what would play, or why there's nothing.
fn draw_in_box(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let lines = match app.current() {
        Some(video) => {
            let pixels = (
                u32::from(area.width) * u32::from(app.cell.0),
                u32::from(area.height) * u32::from(app.cell.1),
            );
            let mut lines = vec![Line::from(clock(video.seconds)).fg(c.text)];
            if let Some(r) = video.best(pixels) {
                lines.push(
                    Line::from(format!("{} {}×{}", r.codec.name(), r.width, r.height)).fg(c.dim),
                );
                if let Some(bytes) = r.bytes {
                    lines.push(Line::from(size(bytes)).fg(c.dim));
                }
            }
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
    let height = (lines.len() as u16).min(area.height);
    let middle = Rect::new(
        area.x,
        area.y + (area.height - height) / 2,
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), middle);
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
            lines.push(
                Line::from(format!(
                    "{} · {} of {}",
                    clock(v.seconds),
                    app.at + 1,
                    app.videos.len()
                ))
                .fg(c.dim),
            );
        }
        None if app.pepper => lines.push(Line::from("In Pepper: videos play in the box").fg(c.ok)),
        None => lines.push(Line::from("Not in Pepper: videos will play in mpv's window").fg(c.red)),
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
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
    for (keys, what) in [("j k", "next/back"), ("r", "more"), ("q", "quit")] {
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
        let mut app = App::new(false, Vec::new(), Default::default());
        app.cell = (9, 18);
        let text = screen(&app, 120, 40);
        assert!(text.contains("No videos yet"));
        assert!(text.contains(" tuitok "));
        assert!(text.contains("Not in Pepper"));
        let last = text.lines().last().unwrap();
        assert!(last.starts_with(" j k next/back  r more  q quit"));
        assert!(last.contains("No browser for the feed"));
    }

    #[test]
    fn a_video_shows_its_author_caption_counts_and_rendition() {
        let app = crate::app::tests::app_with(vec![video("1"), video("2")]);
        let text = screen(&app, 120, 40);
        for shown in [
            "@someone",
            "Some One",
            "first line",
            "second line",
            "♫ original sound",
        ] {
            assert!(text.contains(shown), "{shown}");
        }
        // The counts wrap at the panel's width.
        for shown in ["1.2K plays", "56 likes", "2.5M", "shares"] {
            assert!(text.contains(shown), "{shown}");
        }
        assert!(text.contains("1:15 · 1 of 2"));
        assert!(text.contains("H.265 720×1280"));
        assert!(text.contains("780 KB"));
        assert!(text.contains("2 new videos · Lightpanda · 17 s"));
    }

    #[test]
    fn a_narrow_window_has_no_panel() {
        let mut app = App::new(true, Vec::new(), Default::default());
        app.cell = (9, 18);
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
