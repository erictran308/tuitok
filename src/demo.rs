//! `tuitok --demo`: the real screen filled with made-up videos and covers
//! drawn in code. Nothing is fetched, downloaded or played: for trying the
//! layout, and for screenshots.

use image::{DynamicImage, Rgb, RgbImage};

use crate::comments::Comment;
use crate::video::{Codec, Rendition, Video, VideoId};

/// Made up: handle, name, caption, sound, length in seconds, plays.
const VIDEOS: [(&str, &str, &str, &str, u32, u64); 6] = [
    (
        "made.up.cat",
        "A Cat, Made Up",
        "the 3am zoomies have begun\n\n#cat #fyp",
        "original sound · made.up.cat",
        14,
        2_400_000,
    ),
    (
        "ramen_in_a_hurry",
        "Ramen in a Hurry",
        "4-minute ramen that tastes like 4 hours 🍜",
        "lofi kitchen · ramen_in_a_hurry",
        47,
        870_000,
    ),
    (
        "tiny.gardens",
        "tiny gardens",
        "a whole forest in a jar 🌱 day 41",
        "rain on leaves · tiny.gardens",
        23,
        5_100_000,
    ),
    (
        "skate.dad.99",
        "Skate Dad",
        "first kickflip at 43. it counts.",
        "original sound · skate.dad.99",
        11,
        12_700_000,
    ),
    (
        "rain.window",
        "rain window",
        "8 hours of rain on a window, condensed",
        "rain · rain.window",
        59,
        310_000,
    ),
    (
        "lofi.mornings",
        "lofi mornings",
        "making coffee, slowly ☕",
        "slow morning · lofi.mornings",
        32,
        1_800_000,
    ),
];

/// Each cover's two colours, top and bottom.
const COLOURS: [([u8; 3], [u8; 3]); 6] = [
    ([245, 169, 127], [116, 77, 169]),
    ([238, 190, 190], [202, 158, 230]),
    ([166, 209, 137], [65, 105, 125]),
    ([229, 200, 144], [231, 130, 132]),
    ([140, 170, 238], [48, 52, 70]),
    ([239, 159, 118], [99, 69, 64]),
];

pub fn videos() -> Vec<Video> {
    VIDEOS
        .iter()
        .enumerate()
        .map(
            |(n, &(author, name, caption, music, seconds, plays))| Video {
                id: VideoId::new(&format!("{}", 7_400_000_000_000_000_000u64 + n as u64))
                    .expect("digits"),
                author: author.into(),
                name: name.into(),
                caption: caption.into(),
                music: music.into(),
                seconds,
                plays: Some(plays),
                likes: Some(plays / 13),
                comments: Some(plays / 410),
                shares: Some(plays / 160),
                cover: None,
                renditions: vec![Rendition {
                    codec: Codec::H265,
                    width: 720,
                    height: 1280,
                    bytes: Some(u64::from(seconds) * 52_000),
                    urls: Vec::new(),
                }],
                token: None,
            },
        )
        .collect()
}

/// Made up: who, what, likes.
const COMMENTS: [(&str, &str, u64); 4] = [
    ("night owl", "the zoomies are a whole lifestyle", 18_400),
    ("Tam 🌙", "me at 3am looking for snacks", 9_100),
    ("made.up.dog", "amateur. watch this →", 2_300),
    ("someone", "the sound effects 😭", 640),
];

/// Video `n`'s made-up top comments.
pub fn comments(n: usize) -> Vec<Comment> {
    COMMENTS
        .iter()
        .cycle()
        .skip(n)
        .take(COMMENTS.len())
        .map(|&(name, text, likes)| Comment {
            name: name.into(),
            text: text.into(),
            likes: Some(likes / (n as u64 + 1)),
        })
        .collect()
}

/// Video `n`'s cover: a gradient between two colours, with soft circles of
/// light on it.
pub fn cover(n: usize) -> DynamicImage {
    cover_sized(n, 360, 640)
}

/// Video `n`'s cover, `width`×`height` pixels.
fn cover_sized(n: usize, width: u32, height: u32) -> DynamicImage {
    let (top, bottom) = COLOURS[n % COLOURS.len()];
    let shift = n as f32 * 0.17;
    let circles = [
        (0.30 + shift % 0.4, 0.30, 0.20),
        (0.72, 0.58 - shift % 0.2, 0.26),
        (0.45, 0.82, 0.14 + shift % 0.06),
    ];
    let picture = RgbImage::from_fn(width, height, |x, y| {
        let t = y as f32 / height as f32;
        let mut light = 0.0f32;
        for (cx, cy, r) in circles {
            let (dx, dy) = (x as f32 - cx * width as f32, y as f32 - cy * height as f32);
            let d = (dx * dx + dy * dy).sqrt() / (r * width as f32);
            light += (1.0 - d).max(0.0) * 0.35;
        }
        Rgb(std::array::from_fn(|i| {
            let base = f32::from(top[i]) * (1.0 - t) + f32::from(bottom[i]) * t;
            (base + (255.0 - base) * light.min(0.6)).round() as u8
        }))
    });
    DynamicImage::ImageRgb8(picture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_up_videos_and_covers() {
        let videos = videos();
        assert_eq!(videos.len(), VIDEOS.len());
        assert!(videos.iter().all(|v| v.best((608, 1080)).is_some()));
        assert_eq!(cover(2).height(), 640);
    }

    /// One frame of `--demo` in Pepper for the README's picture: every
    /// cell, and where the cover goes, in `target/hero/` for
    /// `tools/hero.py` (`cargo test -- --ignored export_hero_screen`). The
    /// cell size is the one hero.py draws with, so the box keeps its shape.
    #[test]
    #[ignore = "writes target/hero/ for tools/hero.py"]
    fn export_hero_screen() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::style::{Color, Modifier};

        let (cols, rows) = (112u16, 40u16);
        let cell = (16u16, 34u16);
        let mut app = crate::app::App::demo(true);
        app.cell = cell;
        app.size = (cols, rows);
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let video = crate::ui::areas(Rect::new(0, 0, cols, rows), cell, false).video;
        let cover = crate::ui::cover_area(video, true);
        let hex = |color: Color| match color {
            Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
            _ => None,
        };
        let mut cells = Vec::new();
        for y in 0..rows {
            for x in 0..cols {
                let c = &buffer[(x, y)];
                // Where the cover goes, hero.py draws it.
                let symbol = if cover.contains((x, y).into()) {
                    " "
                } else {
                    c.symbol()
                };
                cells.push(serde_json::json!([
                    symbol,
                    hex(c.fg),
                    hex(c.bg),
                    c.modifier.contains(Modifier::BOLD),
                ]));
            }
        }
        let screen = serde_json::json!({
            "cols": cols,
            "rows": rows,
            "cell": [cell.0, cell.1],
            "bg": hex(app.colors.bg),
            "fg": hex(app.colors.text),
            "cells": cells,
            "cover": [cover.x, cover.y, cover.width, cover.height],
        });
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/hero");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("screen.json"), screen.to_string()).unwrap();
        cover_sized(0, 1080, 1920)
            .save(dir.join("cover.png"))
            .unwrap();
    }
}
