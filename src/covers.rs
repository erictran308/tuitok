//! Cover pictures: each video's still (`Video.cover`, a JPEG on TikTok's
//! image hosts), shown in the box while the video loads, and all the time
//! where it plays in mpv's window. Fetched only from TikTok's image hosts
//! (`cache::client(image_url_allowed)`), at most `MAX_BYTES`, decoded only
//! as a JPEG within [`limits`] with a decoder's panic contained, and
//! encoded for the terminal on a blocking thread: kitty's protocol in
//! Pepper and Ghostty, else what the terminal says it draws, else coloured
//! half blocks. Kept in memory for the videos near the one shown, never on
//! disk; one sent with kitty's protocol is deleted from the terminal once
//! it's dropped ([`KittyIds`]).

use std::cell::Cell;
use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Result, bail};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};
use ratatui::layout::Size;
use ratatui_image::picker::cap_parser::Parser;
use ratatui_image::picker::{Capability, Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::{FontSize, Resize};

use crate::cache;
use crate::video::image_url_allowed;

/// The biggest cover downloaded: TikTok's are 100–300 KB.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// A picture encoded for the terminal, at a size in cells.
pub struct Encoded {
    pub size: (u16, u16),
    pub protocol: Protocol,
    /// Held only to delete it from the terminal once it's dropped.
    _kitty: Option<KittyImage>,
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

/// Numbers of the kitty images dropped since the terminal was last told;
/// see [`KittyIds::deletions`].
type Dropped = Arc<Mutex<Vec<u32>>>;

/// The numbers covers are sent by with kitty's protocol, picked here rather
/// than by the picker, so each can be deleted from the terminal once it's
/// dropped. A terminal speaking it keeps every picture it's sent until told
/// to delete it, and past its quota (320 MB in kitty and Ghostty) drops the
/// ones sent first: a cover as big as the box takes about 16 MB there, and
/// each video and each new size of the box brings one.
pub struct KittyIds {
    /// The number the last cover was given.
    last: u32,
    dropped: Dropped,
}

impl KittyIds {
    /// Numbers from a random start, as the picker's own are: every program
    /// drawing in the terminal's window shares them (two tuitoks in panes
    /// of one tmux), and one's deletions mustn't take the other's pictures.
    pub fn new() -> Self {
        use std::hash::{BuildHasher, RandomState};
        Self::counting_from(RandomState::new().hash_one(std::process::id()) as u32)
    }

    /// Numbers after `last`.
    pub fn counting_from(last: u32) -> Self {
        Self {
            last,
            dropped: Dropped::default(),
        }
    }

    /// A number for the next cover `picker` encodes, if it speaks kitty's
    /// protocol.
    pub fn next(&mut self, picker: &Picker) -> Option<KittyImage> {
        (picker.protocol_type() == ProtocolType::Kitty).then(|| {
            // Never 0, which kitty's protocol takes as no number.
            self.last = self.last.wrapping_add(1).max(1);
            KittyImage {
                id: self.last,
                dropped: self.dropped.clone(),
            }
        })
    }

    /// The code that has the terminal delete the covers dropped since it
    /// was last told, through tmux if it's in one: nothing if none were.
    pub fn deletions(&mut self, tmux: bool) -> String {
        let ids = std::mem::take(&mut *self.dropped.lock().unwrap_or_else(PoisonError::into_inner));
        ids.into_iter().map(|id| kitty_delete(id, tmux)).collect()
    }
}

/// A cover's number with kitty's protocol. Dropping it, wherever that
/// happens (the cover pruned, encoded again at another size, failed, the
/// feed changed, tuitok quitting), adds the number to those the terminal is
/// told to delete; one that was never sent is deleted harmlessly.
pub struct KittyImage {
    id: u32,
    dropped: Dropped,
}

impl Drop for KittyImage {
    fn drop(&mut self) {
        self.dropped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.id);
    }
}

/// The code that has the terminal delete image number `id` of kitty's
/// protocol. `I` deletes the image itself, not only where it's placed;
/// `q=2` asks for no answer, which would come in as keys.
pub fn kitty_delete(id: u32, tmux: bool) -> String {
    let (start, escape, end) = Parser::tmux_start_escape_end(tmux);
    format!("{start}{escape}_Ga=d,d=I,i={id},q=2{escape}\\{end}")
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

/// Encodes `picture` for the terminal, cropped to fill `size` cells; with
/// kitty's protocol, as the image `kitty` numbers.
pub async fn encode(
    picker: Picker,
    picture: Arc<DynamicImage>,
    size: (u16, u16),
    kitty: Option<KittyImage>,
) -> Result<Encoded> {
    tokio::task::spawn_blocking(move || contained(|| encode_now(&picker, &picture, size, kitty)))
        .await?
}

pub fn encode_now(
    picker: &Picker,
    picture: &DynamicImage,
    size: (u16, u16),
    kitty: Option<KittyImage>,
) -> Result<Encoded> {
    let font = picker.font_size();
    let width = u32::from(size.0) * u32::from(font.width);
    let height = u32::from(size.1) * u32::from(font.height);
    let filled = picture.resize_to_fill(width.max(1), height.max(1), FilterType::Triangle);
    let cells = Size::new(size.0, size.1);
    let Some(kitty) = kitty else {
        let protocol = picker.new_protocol(filled, cells, Resize::Fit(None))?;
        return Ok(Encoded {
            size,
            protocol,
            _kitty: None,
        });
    };
    // Made as the picker's `new_protocol` makes it, with tuitok's number
    // rather than a random one: `Resize::Fit` takes a picture that fills its
    // cells to the pixel as it is. Only cells of no pixels (a terminal that
    // didn't say) leave one that doesn't; that cover is made by the picker,
    // as before, under a number of its own that's never deleted.
    if Resize::natural_size(&filled, font) != cells {
        let protocol = picker.new_protocol(filled, cells, Resize::Fit(None))?;
        return Ok(Encoded {
            size,
            protocol,
            _kitty: None,
        });
    }
    let compress = picker
        .capabilities()
        .contains(&Capability::KittyCompression);
    let image = Kitty::new(filled, cells, kitty.id, picker.tmux_detected(), compress)?;
    Ok(Encoded {
        size,
        protocol: Protocol::Kitty(image),
        _kitty: Some(kitty),
    })
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
        let picture = DynamicImage::new_rgb8(90, 160);
        let encoded = encode_now(&picker, &picture, (12, 10), None).unwrap();
        assert_eq!(encoded.size, (12, 10));
    }

    /// A picker for a terminal speaking kitty's protocol, its cells 10 × 20
    /// pixels.
    fn kitty_picker() -> Picker {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        picker
    }

    #[test]
    fn a_cover_for_kitty_is_what_the_pickers_own_encoding_draws_under_tuitoks_number() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let picker = kitty_picker();
        let picture = crate::demo::cover(0);
        let drawn = |encoded: Encoded| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 12, 10));
            ratatui_image::Image::new(&encoded.protocol).render(buf.area, &mut buf);
            buf
        };
        // Without a number it's the picker's own, numbered at random.
        let theirs = drawn(encode_now(&picker, &picture, (12, 10), None).unwrap());
        let sent = theirs[(0, 0)].symbol();
        let number = sent.split(",i=").nth(1).and_then(|s| s.split(',').next());
        let number: u32 = number.unwrap().parse().unwrap();
        let mut ids = KittyIds::counting_from(number.wrapping_sub(1));
        let ours = drawn(encode_now(&picker, &picture, (12, 10), ids.next(&picker)).unwrap());
        assert!(ours == theirs, "drawn as the picker's own");
    }

    #[test]
    fn a_dropped_cover_is_deleted_from_a_kitty_terminal_once_by_its_number() {
        let picker = kitty_picker();
        let picture = DynamicImage::new_rgb8(90, 160);
        let mut ids = KittyIds::counting_from(u32::MAX - 1);
        let first = encode_now(&picker, &picture, (4, 2), ids.next(&picker)).unwrap();
        let second = encode_now(&picker, &picture, (4, 2), ids.next(&picker)).unwrap();
        assert_eq!(ids.deletions(false), "", "nothing dropped yet");
        drop(second);
        drop(first);
        // Past the last number they start again at 1: kitty's protocol takes
        // 0 as none.
        let delete = |id| format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        assert_eq!(ids.deletions(false), delete(1) + &delete(u32::MAX));
        assert_eq!(ids.deletions(false), "", "told once");
    }

    #[test]
    fn inside_tmux_the_code_that_deletes_a_cover_goes_through_it() {
        assert_eq!(
            kitty_delete(1, true),
            "\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=1,q=2\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn terminals_that_keep_no_pictures_get_no_numbers() {
        let picker = Picker::halfblocks();
        let mut ids = KittyIds::counting_from(0);
        assert!(ids.next(&picker).is_none());
        let picture = DynamicImage::new_rgb8(90, 160);
        drop(encode_now(&picker, &picture, (4, 2), ids.next(&picker)).unwrap());
        assert_eq!(ids.deletions(false), "");
    }
}
