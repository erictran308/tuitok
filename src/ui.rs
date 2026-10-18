//! Drawing: the video's box, upright (9:16) and as tall as the window (the
//! whole tab with `f`), and beside it a panel with the video's author,
//! caption, sound, counts, where it's at and top comments, then the keys and
//! the feed's state on the bottom line. Until a video plays in Pepper's box,
//! the box shows its cover, with under it what plays and how it's going.
//! `/` opens a command line on the bottom line, as tuitube's `:`, with For
//! You and Explore's categories in a box over it; `?` a popup over it all:
//! every key, and the settings.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};

use crate::app::{App, FeedState, HelpMenu, HelpTab, Search};
use crate::cache::Download;
use crate::comments::Comments;
use crate::covers::Cover;
use crate::explore;
use crate::pepper;
use crate::theme;
use crate::video::{Video, VideoId};

/// The panel beside the video, in columns, shown in windows at least
/// `PANEL_FROM` columns wide.
const PANEL: u16 = 48;
const PANEL_FROM: u16 = 90;

/// The logo's badge: TikTok's note, white on black as on its icon (true
/// black: `Color::Black` is the palette's colour 0, grey in Catppuccin).
const BADGE_BG: Color = Color::Rgb(0, 0, 0);
const BADGE_FG: Color = Color::Rgb(255, 255, 255);

/// Why videos play in mpv's window in Pepper: multiplexer `name` is between.
fn behind(name: &str) -> String {
    format!("Inside {name}, videos play in mpv's window: Pepper's box needs tuitok outside {name}.")
}

/// Rows of text under the cover in the box.
const TEXT_ROWS: u16 = 3;

/// Around the video in its side's box: the border, and a blank cell inside
/// it, so the video doesn't touch the border or the title in it.
const GAP: Margin = Margin::new(2, 2);

/// The most categories `/` shows at once (tuitube's `MAX_COMPLETIONS`); the
/// rest scroll.
const MAX_OFFERS: usize = 12;

/// Where things go in a window: the box around the video's side (as
/// tuitube's watch page has one), the video's box in it, the panel beside it
/// (in wide windows), the bottom line. `App::box_pixels` lays it out the
/// same.
pub struct Areas {
    pub stage: Option<Rect>,
    pub video: Rect,
    pub panel: Option<Rect>,
    pub bottom: Rect,
}

/// Lays out a window of `area`: the video upright and centred in its side's
/// box, as tall as it allows. With `fullscreen` (`f`, in Pepper) the video
/// takes all but the bottom line, and there's no box or panel.
pub fn areas(area: Rect, cell: (u16, u16), fullscreen: bool) -> Areas {
    let [main, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if fullscreen {
        return Areas {
            stage: None,
            video: main,
            panel: None,
            bottom,
        };
    }
    let (stage, panel) = if main.width >= PANEL_FROM {
        let [stage, panel] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(PANEL)]).areas(main);
        (stage, Some(panel))
    } else {
        (main, None)
    };
    Areas {
        stage: Some(stage),
        video: pepper::portrait_area(stage.inner(GAP), cell),
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
    if let Some(stage) = areas.stage {
        let title = Line::from(vec![
            Span::raw(" "),
            Span::from("▶").fg(c.accent),
            Span::from(format!(" {} ", app.feed_name()))
                .fg(c.text)
                .bold(),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(c.border))
            .title(title);
        frame.render_widget(block, stage);
    }
    frame.render_widget(Block::new().style(Style::new().bg(c.panel)), areas.video);
    draw_in_box(frame, app, areas.video);
    if let Some(panel) = areas.panel {
        draw_panel(frame, app, panel);
    }
    match &app.search {
        // `/`: the command line takes the bottom line, its categories over it.
        Some(search) => {
            command_line(frame, app, search, areas.bottom);
            draw_offers(frame, app, search, areas.bottom, areas.panel);
        }
        None => draw_bottom(frame, app, areas.bottom),
    }
    if let Some(menu) = &app.help {
        draw_help(frame, app, menu, area);
    }
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
            } else if app.player_gone {
                Line::from("The player closed: j or k plays on").fg(c.dim)
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
                FeedState::Fetching(_) => "Fetching the feed…".to_owned(),
                FeedState::Exploring => format!("Fetching {}…", app.feed_name()),
                _ => "No videos yet".to_owned(),
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
    // Sixel and iTerm2 pictures paint over what's drawn on them later: none
    // under the popup or the categories' box (as in tuitube).
    let covered = app.help.is_some() || app.offers_cover_video();
    let cover = cover.filter(|_| !covered || !app.pictures_paint_over());
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
    // tuitube's logo, with TikTok's note for its play button.
    let logo = Line::from(vec![
        Span::raw(" "),
        Span::styled(" ♪ ", Style::new().fg(BADGE_FG).bg(BADGE_BG).bold()),
        Span::styled(" tuitok ", Style::new().fg(c.text).bold()),
    ]);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(c.border))
        .title(logo)
        .padding(Padding::new(2, 2, 1, 1));
    let width = block.inner(area).width;
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
            let (when, bar) = match position(app, v) {
                Some((position, length, paused)) => {
                    let paused = if paused { " · paused" } else { "" };
                    let when = format!("{} / {}{paused}", clock(position), clock(length));
                    (when, Some(progress(app, position, length, width)))
                }
                None => (clock(v.seconds), None),
            };
            let mut spans = vec![Span::from(format!("{when} · ")).fg(c.dim)];
            // How loud, only when it isn't full; muted in the warning colour.
            if app.muted {
                spans.push(Span::from("muted").fg(c.warning));
                spans.push(Span::from(" · ").fg(c.dim));
            } else if app.volume < 100 {
                spans.push(Span::from(format!("volume {}% · ", app.volume)).fg(c.dim));
            }
            let at = format!("{} of {}", app.at + 1, app.videos.len());
            spans.push(Span::from(at).fg(c.dim));
            lines.push(Line::from(spans));
            lines.extend(bar);
            if let Some(name) = app.behind_multiplexer {
                lines.push(Line::default());
                lines.push(Line::from(behind(name)).fg(c.subtext));
            }
            // Last: what doesn't fit is cut off at the bottom.
            if let Some(Comments::Ready(comments)) = app.comments.get(&v.id)
                && !comments.is_empty()
            {
                lines.push(Line::default());
                lines.push(Line::from("Top comments").fg(c.subtext).bold());
                for comment in comments {
                    lines.push(Line::default());
                    let mut who = vec![Span::from(comment.name.clone()).fg(c.accent)];
                    if let Some(likes) = comment.likes {
                        who.push(Span::from(format!(" · ♥ {}", count(likes))).fg(c.dim));
                    }
                    lines.push(Line::from(who));
                    lines.extend(
                        comment
                            .text
                            .lines()
                            .map(|line| Line::from(line.to_owned()).fg(c.text)),
                    );
                }
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

/// The command line `/` opens on the bottom line, as tuitube's `:` (vim's):
/// `/` and what's typed, then its keys on the right, or that nothing
/// matches.
fn command_line(frame: &mut Frame, app: &App, search: &Search, area: Rect) {
    let c = &app.colors;
    let width = usize::from(area.width);
    let mut right = Vec::new();
    if explore::matching(&search.text).is_empty() {
        right.push(Span::from("No category matches ").fg(c.dim));
    } else {
        for (n, (key, what)) in [("↑↓", "move"), ("Enter", "show"), ("Esc", "close")]
            .into_iter()
            .enumerate()
        {
            right.push(Span::from(if n == 0 { " " } else { " · " }).fg(c.dim));
            right.push(Span::from(key).fg(c.accent));
            right.push(Span::from(format!(" {what}")).fg(c.dim));
        }
        right.push(Span::raw(" "));
    }
    let mut right_width: usize = right.iter().map(Span::width).sum();
    if right_width > width / 2 {
        right.clear();
        right_width = 0;
    }
    let typed = tail(&search.text, width.saturating_sub(right_width + 2));
    let used = 2 + Span::raw(typed.to_owned()).width();
    let mut spans = vec![
        Span::from("/").fg(c.accent).bold(),
        Span::from(typed.to_owned()).fg(c.text),
        Span::from("▏").fg(c.accent),
        Span::raw(" ".repeat(width.saturating_sub(used + right_width))),
    ];
    spans.extend(right);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(c.bg)),
        area,
    );
}

/// The end of `text` that fits in `width` columns.
fn tail(text: &str, width: usize) -> &str {
    let mut used = 0;
    for (at, c) in text.char_indices().rev() {
        used += Span::raw(c.encode_utf8(&mut [0; 4]).to_owned()).width();
        if used > width {
            return &text[at + c.len_utf8()..];
        }
    }
    text
}

/// What `/` offers, as tuitube's command line shows what completes it: in a
/// box over the command line (`bottom`), For You and the categories that
/// match what's typed, the feed shown marked, the cursor's row lit, and a
/// scrollbar when they don't all fit; no box when none match. The box goes
/// over the `panel`, as wide and on its bottom edge, so the video plays on
/// beside it; with no panel, over the video's side.
fn draw_offers(frame: &mut Frame, app: &App, search: &Search, bottom: Rect, panel: Option<Rect>) {
    const TITLE: &str = " Categories ";
    let c = &app.colors;
    let offered = explore::matching(&search.text);
    // Room over the command line, less the box's borders.
    let rows = offered
        .len()
        .min(MAX_OFFERS)
        .min(usize::from(bottom.y).saturating_sub(2));
    if rows == 0 {
        return;
    }
    // As wide as the longest of all, so it keeps its width as it narrows;
    // names are ASCII: bytes are cells.
    let names = explore::matching("")
        .iter()
        .map(|offer| explore::name(*offer).len())
        .max()
        .unwrap_or(0);
    // The border, the bar, the mark, the name, a space and the border.
    let fits = (names + 6).max(TITLE.len() + 4) as u16;
    let (x, width) = match panel {
        Some(panel) => (panel.x, panel.width),
        None => (bottom.x, fits.min(bottom.width)),
    };
    let height = rows as u16 + 2;
    let popup = Rect::new(x, bottom.y - height, width, height);
    // As little scrolled as keeps the cursor in view.
    let lit = search.selected;
    let mut scroll = search.scroll.get().min(lit);
    if lit >= scroll + rows {
        scroll = lit + 1 - rows;
    }
    search.scroll.set(scroll);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(c.accent))
        .title(Span::from(TITLE).fg(c.text).bold())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines: Vec<Line> = offered
        .iter()
        .enumerate()
        .skip(scroll)
        .take(rows)
        .map(|(n, offer)| {
            let chosen = n == lit;
            let name = explore::name(*offer);
            let row = Line::from(vec![
                if chosen {
                    Span::from("▌").fg(c.accent)
                } else {
                    Span::raw(" ")
                },
                if *offer == app.category {
                    Span::from("● ").fg(c.accent)
                } else {
                    Span::raw("  ")
                },
                Span::from(name).fg(c.text).bold(),
                // To the box's edge, so the cursor's row is lit across.
                Span::raw(" ".repeat(usize::from(inner.width).saturating_sub(3 + name.len()))),
            ]);
            if chosen {
                row.style(Style::new().bg(c.selected))
            } else {
                row
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    let hidden = offered.len() - rows;
    if hidden > 0 {
        let mut state = ScrollbarState::new(hidden).position(scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .style(Style::new().fg(c.dim)),
            popup.inner(Margin::new(0, 1)),
            &mut state,
        );
    }
}

/// Every key, for the `?` popup's first tab.
const KEYS: [(&str, &str); 10] = [
    ("j  ↓", "the next video"),
    ("k  ↑", "the one before"),
    (
        "/",
        "For You, or one of Explore's categories: type, ↑ ↓, Enter",
    ),
    ("Space", "pause or play"),
    ("h l  ← →  , .", "back or on 5 s"),
    (
        "-  +  /  m",
        "volume down or up (kept for next time)  /  mute",
    ),
    (
        "f",
        "fill the tab in Pepper; elsewhere, mpv's window the screen",
    ),
    ("r", "fetch more videos now"),
    ("?", "these keys, and the settings: the theme"),
    ("q  Esc  Ctrl-c", "quit"),
];

/// The `?` popup, centered, as tuitube's: a tab with every key, and one
/// with the settings; its own keys along its bottom border.
fn draw_help(frame: &mut Frame, app: &App, menu: &HelpMenu, area: Rect) {
    let c = &app.colors;
    let keys = key_lines(c);
    let (settings, cursor) = settings_lines(app, menu);
    let widest = |lines: &[Line]| lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (widest(&keys).max(widest(&settings)) + 3).min(usize::from(area.width)) as u16;
    let height = (keys.len().max(settings.len()) as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let tab = |label: &'static str, active: bool| {
        if active {
            Span::from(label).fg(c.bg).bg(c.accent).bold()
        } else {
            Span::from(label).fg(c.dim)
        }
    };
    let tabs = Line::from(vec![
        Span::raw(" "),
        tab(" Keys ", menu.tab == HelpTab::Keys),
        Span::raw(" "),
        tab(" Settings ", menu.tab == HelpTab::Settings),
        Span::raw(" "),
    ]);
    let hint = match menu.tab {
        HelpTab::Keys => [("j k", "scroll"), ("Tab", "settings"), ("Esc", "close")],
        HelpTab::Settings => [("Enter", "use"), ("Tab", "keys"), ("Esc", "close")],
    };
    let mut spans = vec![Span::raw(" ")];
    for (n, (key, what)) in hint.into_iter().enumerate() {
        if n > 0 {
            spans.push(Span::from(" · ").fg(c.dim));
        }
        spans.push(Span::from(key).fg(c.accent).bold());
        spans.push(Span::from(format!(" {what}")).fg(c.dim));
    }
    spans.push(Span::raw(" "));
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(c.accent))
        .title(tabs)
        .title_bottom(Line::from(spans).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = usize::from(inner.height);
    let (lines, scroll) = match menu.tab {
        HelpTab::Keys => {
            // Kept in range here: the keys stop at the end of the list.
            let scroll = menu.scroll.get().min(keys.len().saturating_sub(rows));
            menu.scroll.set(scroll);
            (keys, scroll)
        }
        // As little as keeps the cursor in view.
        HelpTab::Settings => (settings, (cursor + 1).saturating_sub(rows)),
    };
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
}

/// The keys tab: every key, and what it does.
fn key_lines(c: &theme::Colors) -> Vec<Line<'static>> {
    let key_width = KEYS.iter().map(|(k, _)| Span::raw(*k).width()).max();
    KEYS.iter()
        .map(|(key, what)| {
            let pad = " ".repeat(key_width.unwrap_or(0) - Span::raw(*key).width());
            Line::from(vec![
                Span::from(format!(" {key}{pad}  ")).fg(c.accent),
                Span::from(*what).fg(c.text),
            ])
        })
        .collect()
}

/// The settings tab: the themes, the one in use dotted, the one under the
/// cursor lit; and the line the cursor is on.
fn settings_lines(app: &App, menu: &HelpMenu) -> (Vec<Line<'static>>, usize) {
    let c = &app.colors;
    let mut lines = vec![Line::from(" Theme").fg(c.dim).bold()];
    let mut cursor = 0;
    for (row, (id, name)) in theme::names().iter().enumerate() {
        let chosen = row == menu.selected;
        let line = Line::from(vec![
            Span::from(if chosen { "▌" } else { " " }).fg(c.accent),
            Span::from(if *id == app.theme { " ● " } else { " ○ " }).fg(c.accent),
            Span::from(name.clone()).fg(c.text),
        ]);
        if chosen {
            cursor = lines.len();
            lines.push(line.bg(c.selected));
        } else {
            lines.push(line);
        }
    }
    if let Some(e) = &menu.error {
        lines.push(Line::default());
        lines.push(Line::from(format!(" {e}")).fg(c.red));
    }
    (lines, cursor)
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
    frame.render_widget(Paragraph::new(hints(app, keys.width)), keys);
    frame.render_widget(Paragraph::new(status).alignment(Alignment::Right), right);
}

/// The keys, each in the accent colour, what it does dimmed. The last is
/// always there; one before it that doesn't fit in `width` is left out
/// whole, and a shorter one after it can still fit.
fn hints(app: &App, width: u16) -> Line<'static> {
    let c = &app.colors;
    let keys = [
        ("j k", "next/back"),
        ("/", "category"),
        ("space", "pause"),
        ("m", "mute"),
        ("?", "help"),
        ("- +", "volume"),
        ("f", "fill"),
        ("r", "more"),
        ("q", "quit"),
    ];
    // The last (how to quit) is always there; the others fit in before it.
    let (last, rest) = keys.split_last().expect("there are hints");
    let cells = |s: &str| Span::raw(s.to_owned()).width();
    let wide = |(keys, what): &(&str, &str)| 2 + cells(keys) + 1 + cells(what);
    // The first one's gap is one space, not two.
    let mut room = (usize::from(width) + 1).saturating_sub(wide(last));
    let mut shown = Vec::new();
    for hint in rest {
        if wide(hint) <= room {
            room -= wide(hint);
            shown.push(hint);
        }
    }
    shown.push(last);
    let mut spans = Vec::new();
    for (keys, what) in shown {
        spans.push(Span::from(if spans.is_empty() { " " } else { "  " }));
        spans.push(Span::from(*keys).fg(c.accent));
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
            let videos = match app.batch_size {
                1 => "1 video".to_owned(),
                n => format!("{n} videos"),
            };
            let mut said = format!("Fetching {videos} with {}…", engine.name());
            let so_far = app.found_so_far();
            if so_far > 0 {
                said.push_str(&format!(" {so_far} so far"));
            }
            Line::from(said).fg(c.dim)
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
        FeedState::Exploring => Line::from(format!("Fetching {}…", app.feed_name())).fg(c.dim),
        FeedState::Explored { new, took } => Line::from(format!(
            "{new} new videos · {} · {:.0} s",
            app.feed_name(),
            took.as_secs_f64()
        ))
        .fg(c.dim),
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
        assert!(text.contains("╭  ♪  tuitok ─"));
        assert!(text.contains("Not in Pepper"));
        let last = text.lines().last().unwrap();
        // The status is long: the hints that don't fit are left out, but
        // not how to quit.
        assert!(last.starts_with(" j k next/back  / category  space pause  m mute  q quit"));
        assert!(last.contains("No browser for the feed"));
        app.feed = FeedState::Failed("x".into());
        let last = screen(&app, 120, 40).lines().last().unwrap().to_owned();
        assert!(last.starts_with(
            " j k next/back  / category  space pause  m mute  ? help  - + volume  f fill  q quit"
        ));
    }

    #[test]
    fn slash_opens_a_command_line_with_the_categories_over_it() {
        let mut app = crate::app::tests::app_with(vec![video("1")]);
        let line = |text: &str, shown: &str| {
            text.lines()
                .find(|line| line.contains(shown))
                .unwrap_or_else(|| panic!("{shown}\n{text}"))
                .to_owned()
        };
        // Closed: the feed's name is the title, and `/` is among the keys.
        let text = screen(&app, 120, 40);
        assert!(text.lines().next().unwrap().starts_with("╭ ▶ For You ──"));
        assert!(
            text.lines()
                .last()
                .unwrap()
                .starts_with(" j k next/back  / category  ")
        );
        // Open: the command line takes the bottom line, as tuitube's `:`, and
        // over it, on the panel's bottom edge and as wide, a box of what
        // matches, the feed shown marked and the cursor's row lit across.
        app.search = Some(Search {
            text: "fo".into(),
            selected: 1,
            ..Search::default()
        });
        let text = screen(&app, 120, 40);
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[39].starts_with("/fo▏ "), "{}", rows[39]);
        assert!(
            rows[39].ends_with(" ↑↓ move · Enter show · Esc close "),
            "{}",
            rows[39]
        );
        // The panel starts at column 72 of 120.
        let panel = |row: &str| row.chars().skip(72).collect::<String>();
        let top = panel(rows[35]);
        assert!(
            top.starts_with("╭ Categories ─") && top.ends_with("─╮"),
            "{top}"
        );
        assert!(panel(rows[36]).starts_with("│ ● For You "), "{}", rows[36]);
        assert!(panel(rows[37]).starts_with("│▌  Food "), "{}", rows[37]);
        assert!(panel(rows[38]).starts_with("╰─"), "{}", rows[38]);
        assert_eq!(bg_of(&app, (120, 40), "Food", 0), app.colors.selected);
        assert_eq!(bg_of(&app, (120, 40), "Food", 42), app.colors.selected);
        assert!(!text.contains("Comedy"));
        // The video's side isn't covered.
        assert!(
            !rows[36]
                .chars()
                .take(72)
                .collect::<String>()
                .contains("For You")
        );
        // A narrow window has no panel: the box is over the video's side,
        // as wide as the longest name needs.
        let narrow = screen(&app, 80, 40);
        let rows: Vec<&str> = narrow.lines().collect();
        assert!(
            rows[35].starts_with("╭ Categories ─────────╮"),
            "{}",
            rows[35]
        );
        assert!(rows[37].starts_with("│▌  Food "), "{}", rows[37]);
        // Nothing matches: no box, and the command line says so.
        app.search = Some(Search {
            text: "zzz".into(),
            ..Search::default()
        });
        let text = screen(&app, 120, 40);
        assert!(!text.contains("Categories"));
        assert!(
            text.lines()
                .last()
                .unwrap()
                .ends_with("No category matches ")
        );
        // Everything: 12 rows at most, scrolled to keep the cursor in view,
        // with a scrollbar; the scroll stays while the cursor moves in view.
        app.search = Some(Search {
            selected: 21,
            ..Search::default()
        });
        let text = screen(&app, 120, 40);
        let technology = line(&text, "Technology");
        assert!(technology.contains("│▌  Technology") && technology.contains('█'));
        assert!(!text.contains("● For You") && !text.contains("Daily Life"));
        app.search.as_mut().unwrap().selected = 15;
        let text = screen(&app, 120, 40);
        assert_eq!(app.search.as_ref().unwrap().scroll.get(), 10);
        assert!(line(&text, " Food").contains("│▌  Food"));
        assert!(text.contains("Beauty Care") && !text.contains("Daily Life"));
        // A category shown: its name is the title, and the status says so.
        app.search = None;
        app.category = Some(explore::CATEGORIES[2]);
        app.feed = FeedState::Explored {
            new: 15,
            took: std::time::Duration::from_millis(600),
        };
        let text = screen(&app, 120, 40);
        assert!(text.lines().next().unwrap().starts_with("╭ ▶ Comedy ──"));
        assert!(
            text.lines()
                .last()
                .unwrap()
                .ends_with("15 new videos · Comedy · 1 s")
        );
        app.feed = FeedState::Exploring;
        app.videos.clear();
        assert!(screen(&app, 120, 40).contains("Fetching Comedy…"));
        // With f, the same.
        app.fullscreen = true;
        app.search = Some(Search::default());
        assert!(
            screen(&app, 120, 40)
                .lines()
                .last()
                .unwrap()
                .starts_with("/▏ ")
        );
    }

    /// The background `dx` cells after where `shown` starts on screen.
    fn bg_of(app: &App, (width, height): (u16, u16), shown: &str, dx: u16) -> Color {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..height {
            let (mut row, mut starts) = (String::new(), Vec::new());
            for x in 0..width {
                starts.push(row.len());
                row.push_str(buffer[(x, y)].symbol());
            }
            if let Some(at) = row.find(shown) {
                let x = starts.iter().position(|start| *start == at).unwrap() as u16;
                return buffer[(x + dx, y)].bg;
            }
        }
        panic!("{shown} isn't shown");
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
        // How loud, when it isn't full.
        app.volume = 80;
        assert!(screen(&app, 120, 40).contains("0:03 / 1:15 · paused · volume 80% · 1 of 2"));
        app.muted = true;
        assert!(screen(&app, 120, 40).contains("0:03 / 1:15 · paused · muted · 1 of 2"));
        app.muted = false;
        // Why a video doesn't play.
        app.playing = None;
        app.play_error = Some("It can't play: loading failed".into());
        assert!(screen(&app, 120, 40).contains("It can't play: loading failed"));
        // Its window closed: none opens again until a key asks.
        app.play_error = None;
        app.player_gone = true;
        assert!(screen(&app, 120, 40).contains("The player closed: j or k plays on"));
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
        // The video's side is boxed, as tuitube's watch page, with the
        // video in it, under its title.
        let laid = areas(Rect::new(0, 0, 120, 40), app.cell, false);
        let stage = laid.stage.unwrap();
        assert!(stage.inner(GAP).contains(laid.video.as_position()));
        // A blank row between the video and the bar in the top border, and
        // one under it.
        assert!(laid.video.y >= stage.y + 2);
        assert!(laid.video.right() < stage.right() - 1 && laid.video.bottom() < stage.bottom() - 1);
        assert!(text.contains("╭ ▶ For You ─"));
        // f: the box takes all but the bottom line, and the panel goes.
        let filled = areas(Rect::new(0, 0, 120, 40), app.cell, true);
        assert_eq!(
            (filled.stage, filled.video, filled.panel),
            (None, Rect::new(0, 0, 120, 39), None)
        );
        app.fullscreen = true;
        let text = screen(&app, 120, 40);
        assert!(!text.contains("@made.up.cat") && !text.contains("For You"));
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
        assert!(!text.contains("tuitok ─"));
    }

    #[test]
    fn the_popup_lists_every_key_then_the_settings() {
        use std::cell::Cell;
        let mut app = crate::app::tests::app_with(vec![video("1")]);
        app.help = Some(HelpMenu {
            tab: HelpTab::Keys,
            scroll: Cell::new(99),
            selected: 1,
            error: None,
        });
        let text = screen(&app, 120, 40);
        let line = |text: &str, shown: &str| {
            text.lines()
                .find(|line| line.contains(shown))
                .unwrap_or_else(|| panic!("{shown}\n{text}"))
                .to_owned()
        };
        assert!(line(&text, " Keys ").contains(" Settings "), "the tabs");
        for (key, what) in KEYS {
            assert!(line(&text, what).contains(key), "{key}");
        }
        assert!(text.contains("j k scroll · Tab settings · Esc close"));
        assert_eq!(app.help.as_ref().unwrap().scroll.get(), 0, "kept in range");
        // The panel's still there, under it; the bottom line names `?`.
        assert!(text.contains("@someone"));
        assert!(text.lines().last().unwrap().contains("? help"));
        // The settings tab: the theme in use dotted, the cursor's lit.
        let menu = app.help.as_mut().unwrap();
        menu.tab = HelpTab::Settings;
        menu.error = Some("settings.toml isn't valid TOML, so it wasn't changed".into());
        let text = screen(&app, 120, 40);
        assert!(
            line(&text, "Catppuccin Frappé").contains("▌ ○"),
            "the cursor"
        );
        assert!(
            line(&text, "Catppuccin Mocha").contains(" ● "),
            "the one in use"
        );
        assert!(text.contains("Rosé Pine") && text.contains("isn't valid TOML"));
        assert!(text.contains("Enter use · Tab keys · Esc close"));
        assert!(!text.contains("the next video"));
        // A short window: the cursor kept in view.
        app.help.as_mut().unwrap().selected = theme::names().len() - 1;
        let text = screen(&app, 120, 8);
        assert!(line(&text, "Rosé Pine").contains("▌"));
    }

    #[test]
    fn top_comments_come_last_in_the_panel() {
        use crate::comments::Comment;
        let mut app = crate::app::tests::app_with(vec![video("1")]);
        let id = VideoId::new("1").unwrap();
        app.comments.insert(id.clone(), Comments::Loading);
        assert!(!screen(&app, 120, 40).contains("Top comments"));
        let comment = Comment {
            name: "Linh".into(),
            text: "trời cây chôm chôm nhìu quả thế".into(),
            likes: Some(866),
        };
        app.comments.insert(id, Comments::Ready(vec![comment]));
        let text = screen(&app, 120, 40);
        let at = |shown: &str| text.find(shown).unwrap_or_else(|| panic!("{shown}"));
        assert!(at("1:15 · 1 of 1") < at("Top comments"));
        assert!(at("Top comments") < at("Linh · ♥ 866"));
        assert!(at("Linh · ♥ 866") < at("trời cây chôm chôm"));
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
