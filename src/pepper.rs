//! Pepper: a Ghostty build that shows video inside the terminal, straight
//! from the graphics chip (`../pepper`). There tuitok will play each video
//! through Pepper's player, pepper-play, in a box of cells it places with
//! `ESC ] 777 ; pepper ; place ; TOKEN ; COL ; ROW ; COLS ; ROWS ESC \`
//! (`../pepper/pepper-play/README.md`), as tuitube does
//! (`../tuitube/src/pepper.rs`, where the token and the sequences come from
//! when the player lands): whether tuitok runs in Pepper, the box's shape,
//! the token that ties pepper-play's picture to the box, and the sequences
//! that place the box and take it away.

use std::ffi::OsString;

use ratatui::layout::Rect;

/// The token's random bytes (32 hex digits).
const TOKEN_BYTES: usize = 16;

/// What a terminal multiplexer running tuitok sets: tmux, GNU screen,
/// zellij. Each would need every escape sequence wrapped to pass it on, and
/// screen may log them (`screen -L`).
const MULTIPLEXERS: [&str; 3] = ["TMUX", "STY", "ZELLIJ"];

/// Pepper's port, from `PEPPER_PORT`, when tuitok runs in Pepper: not
/// through a multiplexer.
pub fn port() -> Option<OsString> {
    port_from(|name| std::env::var_os(name))
}

fn port_from(var: impl Fn(&str) -> Option<OsString>) -> Option<OsString> {
    if MULTIPLEXERS.iter().any(|name| var(name).is_some()) {
        return None;
    }
    var("PEPPER_PORT").filter(|port| !port.is_empty())
}

/// A token for one player's picture: random hex digits, as Pepper takes
/// them (1–64 of `[A-Za-z0-9_-]`). It goes to pepper-play over its socket
/// only, and to Pepper only in tuitok's own sequences to its tab. `None` if
/// the system's randomness can't be read.
pub fn new_token() -> Option<String> {
    use std::io::Read;
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Puts the picture of `token`'s player over `area`.
pub fn place(token: &str, area: Rect) -> String {
    format!(
        "\x1b]777;pepper;place;{token};{};{};{};{}\x1b\\",
        area.x, area.y, area.width, area.height
    )
}

/// Takes the picture of `token`'s player away.
pub fn remove(token: &str) -> String {
    format!("\x1b]777;pepper;remove;{token}\x1b\\")
}

/// What Pepper was last told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub token: String,
    pub area: Rect,
}

/// The tallest 9:16 box that fits in `area` for cells of `cell` pixels,
/// centred in it: TikTok's videos are upright (576×1024, 1080×1920).
/// Pepper keeps a picture's shape in any box, so a video of another shape
/// gets black bars rather than being stretched.
pub fn portrait_area(area: Rect, cell: (u16, u16)) -> Rect {
    let (cw, ch) = (f64::from(cell.0.max(1)), f64::from(cell.1.max(1)));
    let mut rows = area.height;
    let mut cols = (f64::from(rows) * ch * 9.0 / 16.0 / cw).round() as u16;
    if cols > area.width {
        cols = area.width;
        rows = (f64::from(cols) * cw * 16.0 / 9.0 / ch).round() as u16;
    }
    let cols = cols.max(1).min(area.width);
    let rows = rows.max(1).min(area.height);
    Rect::new(
        area.x + (area.width - cols) / 2,
        area.y + (area.height - rows) / 2,
        cols,
        rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_in_pepper_outside_multiplexers() {
        let env = |vars: &'static [(&str, &str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert_eq!(
            port_from(env(&[("PEPPER_PORT", "io.github.erictran308.pepper.42")])),
            Some("io.github.erictran308.pepper.42".into())
        );
        assert_eq!(port_from(env(&[])), None);
        assert_eq!(port_from(env(&[("PEPPER_PORT", "")])), None);
        for multiplexer in MULTIPLEXERS {
            assert_eq!(
                port_from(env(match multiplexer {
                    "TMUX" => &[
                        ("PEPPER_PORT", "x.1"),
                        ("TMUX", "/tmp/tmux-501/default,1,0")
                    ],
                    "STY" => &[("PEPPER_PORT", "x.1"), ("STY", "123.ttys001.mac")],
                    _ => &[("PEPPER_PORT", "x.1"), ("ZELLIJ", "0")],
                })),
                None,
                "{multiplexer}"
            );
        }
    }

    #[test]
    fn tokens_are_fresh_hex_and_sequences_carry_only_cells() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 2 * TOKEN_BYTES);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        let area = Rect::new(2, 3, 40, 11);
        assert_eq!(
            place("ab", area),
            "\x1b]777;pepper;place;ab;2;3;40;11\x1b\\"
        );
        assert_eq!(remove("ab"), "\x1b]777;pepper;remove;ab\x1b\\");
    }

    #[test]
    fn boxes_are_9_by_16_and_centred() {
        // Cells 9×18 px: 40 rows are 720 px tall, so 405 px wide, 45 columns.
        assert_eq!(
            portrait_area(Rect::new(0, 0, 120, 40), (9, 18)),
            Rect::new(37, 0, 45, 40)
        );
        // Too narrow for that: 30 columns are 270 px, so 480 px tall, 26.7 rows.
        assert_eq!(
            portrait_area(Rect::new(0, 0, 30, 40), (9, 18)),
            Rect::new(0, 6, 30, 27)
        );
        // Never outside the area, even a tiny or empty one.
        let tiny = portrait_area(Rect::new(4, 4, 3, 1), (9, 18));
        assert!(tiny.width <= 3 && tiny.height == 1 && tiny.x >= 4);
        assert_eq!(portrait_area(Rect::new(0, 0, 0, 0), (9, 18)).width, 0);
    }
}
