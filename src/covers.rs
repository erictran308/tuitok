//! Cover pictures: each video's still (`Video.cover`, a JPEG on TikTok's
//! image hosts), shown in the box while the video loads, and all the time
//! where it plays in mpv's window. Fetched only from TikTok's image hosts
//! (`cache::client(image_url_allowed)`), at most `MAX_BYTES`, decoded only
//! as a JPEG within [`limits`] with a decoder's panic contained, and
//! encoded for the terminal on a blocking thread: kitty's protocol in
//! Pepper and Ghostty, else what the terminal says it draws, else coloured
//! half blocks. Kept in memory for the videos near the one shown, never on
//! disk.

use std::cell::Cell;
use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use anyhow::{Result, bail};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};
use ratatui::layout::Size;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Resize};

use crate::cache;
use crate::video::image_url_allowed;

/// The biggest cover downloaded: TikTok's are 100–300 KB.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// A picture encoded for the terminal, at a size in cells.
#[derive(Clone)]
pub struct Encoded {
    pub size: (u16, u16),
    pub protocol: Protocol,
}

/// A video's cover, as far as it's got.
pub enum Cover {
    Loading,
    Ready {
        picture: Arc<DynamicImage>,
        encoded: Option<Encoded>,
        /// An encoding is under way.
        encoding: bool,
    },
    Failed,
}

/// How the terminal draws pictures, decided without asking where that can
/// be: asking means waiting for an answer, and a terminal that never
/// answers leaves ratatui-image's reader blocked on the keyboard, where it
/// takes the first key pressed.
/// - Ghostty (and so Pepper) and kitty say so by their environment and draw
///   kitty's protocol, with the cell size from the window's.
/// - Through tmux the terminal is asked, through tmux: ratatui-image wraps
///   its pictures for it (and turns its passthrough on, `tmux.rs`).
/// - Through another multiplexer (screen, zellij, herdr), which passes
///   neither the question nor kitty's pictures: coloured half blocks, which
///   any passes.
/// - Others are asked, else get half blocks.
///
/// Call after the terminal is set up and before keys are read.
pub fn picker() -> Picker {
    let multiplexer = crate::pepper::multiplexer(|name| std::env::var_os(name));
    let forced = |protocol| {
        cell_size().map(|font| {
            // Deprecated in favour of asking the terminal; it's the only
            // way to give a picker the cell size without asking.
            #[allow(deprecated)]
            let mut picker = Picker::from_fontsize(font);
            picker.set_protocol_type(protocol);
            picker
        })
    };
    let asked = || Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    match multiplexer {
        Some("tmux") => asked(),
        Some(_) => forced(ProtocolType::Halfblocks).unwrap_or_else(Picker::halfblocks),
        None if kitty_terminal(|name| std::env::var(name).ok()) => {
            forced(ProtocolType::Kitty).unwrap_or_else(asked)
        }
        None => asked(),
    }
}

/// The environment says the terminal is Ghostty (Pepper too) or kitty.
fn kitty_terminal(env: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    set("TERM_PROGRAM").is_some_and(|p| p.eq_ignore_ascii_case("ghostty"))
        || set("TERM").is_some_and(|t| t == "xterm-ghostty" || t == "xterm-kitty")
        || set("KITTY_WINDOW_ID").is_some()
}

/// The terminal's cell size in pixels, from the window size it reports.
fn cell_size() -> Option<FontSize> {
    // SAFETY: TIOCGWINSZ only fills in the winsize given; on a stdout that
    // isn't a terminal it fails and nothing is read.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    let (w, h, cols, rows) = (size.ws_xpixel, size.ws_ypixel, size.ws_col, size.ws_row);
    (w > 0 && h > 0 && cols > 0 && rows > 0).then(|| FontSize::new(w / cols, h / rows))
}

/// Downloads a cover and decodes it, off the event loop.
pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<DynamicImage> {
    if !image_url_allowed(url) {
        bail!("not one of TikTok's pictures");
    }
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, cache::USER_AGENT)
        .header(reqwest::header::REFERER, cache::REFERER)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("HTTP {}", response.status().as_u16());
    }
    let data = cache::read_capped(response, MAX_BYTES, "the picture").await?;
    tokio::task::spawn_blocking(move || contained(|| decode(&data))).await?
}

/// Encodes `picture` for the terminal, cropped to fill `size` cells.
pub async fn encode(
    picker: Picker,
    picture: Arc<DynamicImage>,
    size: (u16, u16),
) -> Result<Encoded> {
    tokio::task::spawn_blocking(move || contained(|| encode_now(&picker, &picture, size))).await?
}

pub fn encode_now(picker: &Picker, picture: &DynamicImage, size: (u16, u16)) -> Result<Encoded> {
    let font = picker.font_size();
    let width = u32::from(size.0) * u32::from(font.width);
    let height = u32::from(size.1) * u32::from(font.height);
    let filled = picture.resize_to_fill(width.max(1), height.max(1), FilterType::Triangle);
    let protocol = picker.new_protocol(filled, Size::new(size.0, size.1), Resize::Fit(None))?;
    Ok(Encoded { size, protocol })
}

/// Caps for decoding: TikTok's covers are at most 1080×1920, so a much
/// bigger picture is only a way to run memory out.
fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits
}

/// Decodes a JPEG, the format TikTok's covers come in, and nothing else.
fn decode(data: &[u8]) -> Result<DynamicImage> {
    let mut reader = image::ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    if reader.format() != Some(ImageFormat::Jpeg) {
        bail!("not a JPEG");
    }
    reader.limits(limits());
    Ok(reader.decode()?)
}

thread_local! {
    /// Set while this thread decodes or encodes a picture.
    static DECODING: Cell<bool> = const { Cell::new(false) };
}

/// The panic is in a picture's decoder, where it's caught: the cover is
/// left out and tuitok carries on (`main`'s panic hook asks).
pub fn panic_is_contained() -> bool {
    DECODING.with(Cell::get)
}

fn contained<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    DECODING.with(|d| d.set(true));
    let result = catch_unwind(AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the picture couldn't be decoded")));
    DECODING.with(|d| d.set(false));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_jpegs_within_limits_are_decoded() {
        let mut jpeg = Vec::new();
        DynamicImage::new_rgb8(36, 64)
            .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        assert_eq!(decode(&jpeg).unwrap().height(), 64);
        assert!(decode(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").is_err());
        assert!(decode(b"RIFF\0\0\0\0WEBPVP8 ").is_err());
        assert!(decode(b"<html>").is_err());
        let mut huge = Vec::new();
        DynamicImage::new_rgb8(3000, 10)
            .write_to(&mut Cursor::new(&mut huge), ImageFormat::Jpeg)
            .unwrap();
        assert!(decode(&huge).is_err());
    }

    #[test]
    fn a_panic_while_decoding_is_contained() {
        let result: Result<()> = contained(|| panic!("a broken decoder"));
        assert!(result.is_err());
        assert!(!panic_is_contained());
    }

    #[test]
    fn kitty_by_its_environment() {
        let env = |vars: &'static [(&str, &str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(kitty_terminal(env(&[("TERM_PROGRAM", "ghostty")])));
        assert!(kitty_terminal(env(&[("TERM", "xterm-kitty")])));
        assert!(!kitty_terminal(env(&[("TERM_PROGRAM", "Apple_Terminal")])));
        assert!(!kitty_terminal(env(&[("KITTY_WINDOW_ID", "")])));
    }

    #[test]
    fn encoding_fills_the_size_asked_for() {
        let picker = Picker::halfblocks();
        let encoded = encode_now(&picker, &DynamicImage::new_rgb8(90, 160), (12, 10)).unwrap();
        assert_eq!(encoded.size, (12, 10));
    }
}
