//! `tuitok --demo`: the real screen filled with made-up videos and covers
//! drawn in code. Nothing is fetched, downloaded or played: for trying the
//! layout, and for screenshots.

use image::{DynamicImage, Rgb, RgbImage};

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

/// Video `n`'s cover: a gradient between two colours, with soft circles of
/// light on it.
pub fn cover(n: usize) -> DynamicImage {
    let (width, height) = (360u32, 640u32);
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
}
