//! Pepper: a Ghostty build that shows video inside the terminal, straight
//! from the graphics chip (`../pepper`). There tuitok will play each video
//! through Pepper's player, pepper-play, in a box of cells it places with
//! `ESC ] 777 ; pepper ; place ; TOKEN ; COL ; ROW ; COLS ; ROWS ESC \`
//! (`../pepper/pepper-play/README.md`), as tuitube does
//! (`../tuitube/src/pepper.rs`, where the token and the sequences come from
//! when the player lands): whether tuitok runs in Pepper, the box's shape,
//! the token that ties pepper-play's picture to the box, and the sequences
//! that place the box and take it away.
//!
//! A popup over the picture: a Pepper that lists `holes` in
//! `PEPPER_FEATURES` (`takes_holes`) takes up to four boxes of cells after
//! the box's, `;COL,ROW,COLS,ROWS` each, where the video leaves the cells
//! to the terminal, so the picture plays on around what's drawn over it
//! (`holes`). An older one refuses a place with holes (and keeps the box
//! where it was), so there the picture is taken away while a popup is
//! open.

use std::ffi::OsString;

use ratatui::layout::Rect;

/// The token's random bytes (32 hex digits).
const TOKEN_BYTES: usize = 16;

/// What a terminal multiplexer running tuitok sets, and its name: tmux, GNU
/// screen, zellij, herdr. Each would need every escape sequence wrapped (and
/// its cells moved to where its pane is) to pass it on, and screen may log
/// them (`screen -L`).
const MULTIPLEXERS: [(&str, &str); 4] = [
    ("TMUX", "tmux"),
    ("STY", "screen"),
    ("ZELLIJ", "zellij"),
    ("HERDR_PANE_ID", "herdr"),
];

/// Pepper's port, from `PEPPER_PORT`, when tuitok runs in Pepper: not
/// through a multiplexer.
pub fn port() -> Option<OsString> {
    port_from(|name| std::env::var_os(name))
}

fn port_from(var: impl Fn(&str) -> Option<OsString>) -> Option<OsString> {
    if multiplexer(&var).is_some() {
        return None;
    }
    var("PEPPER_PORT").filter(|port| !port.is_empty())
}

/// The terminal multiplexer running tuitok, by name, if one does.
pub fn multiplexer(var: impl Fn(&str) -> Option<OsString>) -> Option<&'static str> {
    MULTIPLEXERS
        .iter()
        .find(|(variable, _)| var(variable).is_some())
        .map(|&(_, name)| name)
}

/// tuitok runs in Pepper, but through a multiplexer, by name: Pepper is
/// there, and its box can't be placed, so videos play in mpv's window.
pub fn behind_multiplexer() -> Option<&'static str> {
    let var = |name: &str| std::env::var_os(name);
    var("PEPPER_PORT").filter(|port| !port.is_empty())?;
    multiplexer(var)
}

/// Whether this Pepper takes holes in a box: `PEPPER_FEATURES` lists
/// `holes` (comma separated). Not set: an older Pepper.
pub fn takes_holes() -> bool {
    holes_from(std::env::var_os("PEPPER_FEATURES"))
}

fn holes_from(features: Option<OsString>) -> bool {
    features
        .and_then(|value| value.into_string().ok())
        .is_some_and(|value| value.split(',').any(|feature| feature.trim() == "holes"))
}

/// The most holes Pepper takes in a box.
pub const MAX_HOLES: usize = 4;

/// What of `area` the boxes in `over` (the popups drawn over the picture)
/// cover, as holes for `place`: each cut to the box, those outside it or
/// inside another left out, and while there are more than Pepper takes,
/// the two that together take the least more of the picture made one (one
/// around them all could take most of it). As tuitube's.
pub fn holes(area: Rect, over: &[Rect]) -> Vec<Rect> {
    let mut holes: Vec<Rect> = over
        .iter()
        .map(|place| place.intersection(area))
        .filter(|hole| !hole.is_empty())
        .collect();
    drop_covered(&mut holes);
    let cells = |r: Rect| i64::from(r.area());
    while holes.len() > MAX_HOLES {
        let n = holes.len();
        let (i, j) = (0..n)
            .flat_map(|i| (i + 1..n).map(move |j| (i, j)))
            .min_by_key(|&(i, j)| {
                cells(holes[i].union(holes[j])) - cells(holes[i]) - cells(holes[j])
            })
            .expect("more than one");
        holes[i] = holes[i].union(holes[j]);
        holes.swap_remove(j);
        drop_covered(&mut holes);
    }
    holes
}

/// Leaves out the holes inside another (the same one twice, or one a merge
/// took in): they'd take one of Pepper's few and cut nothing more.
fn drop_covered(holes: &mut Vec<Rect>) {
    let mut kept: Vec<Rect> = Vec::with_capacity(holes.len());
    let inside = |a: Rect, b: Rect| a.intersection(b) == a;
    for hole in holes.drain(..) {
        if kept.iter().any(|&k| inside(hole, k)) {
            continue;
        }
        kept.retain(|&k| !inside(k, hole));
        kept.push(hole);
    }
    *holes = kept;
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

/// Puts the picture of `token`'s player over `area`, but for `holes` (none
/// for a Pepper that doesn't take them: `takes_holes`).
pub fn place(token: &str, area: Rect, holes: &[Rect]) -> String {
    let holes: String = holes
        .iter()
        .map(|h| format!(";{},{},{},{}", h.x, h.y, h.width, h.height))
        .collect();
    format!(
        "\x1b]777;pepper;place;{token};{};{};{};{}{holes}\x1b\\",
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
    pub holes: Vec<Rect>,
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
        for (variable, name) in MULTIPLEXERS {
            let var = |asked: &str| {
                (asked == variable || asked == "PEPPER_PORT").then(|| OsString::from("x"))
            };
            assert_eq!(port_from(var), None, "{name}");
            assert_eq!(multiplexer(var), Some(name));
        }
    }

    #[test]
    fn multiplexers_by_their_variables() {
        let env = |vars: &'static [(&str, &str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert_eq!(
            multiplexer(env(&[("TMUX", "/tmp/tmux-501/default,1,0")])),
            Some("tmux")
        );
        let herdr = env(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w12:p2")]);
        assert_eq!(multiplexer(herdr), Some("herdr"));
        assert_eq!(multiplexer(env(&[("TERM_PROGRAM", "ghostty")])), None);
    }

    #[test]
    fn tokens_are_fresh_hex_and_sequences_carry_only_cells() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 2 * TOKEN_BYTES);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        let area = Rect::new(2, 3, 40, 11);
        assert_eq!(
            place("ab", area, &[]),
            "\x1b]777;pepper;place;ab;2;3;40;11\x1b\\"
        );
        assert_eq!(
            place(
                "ab",
                area,
                &[Rect::new(10, 5, 20, 4), Rect::new(0, 0, 160, 1)]
            ),
            "\x1b]777;pepper;place;ab;2;3;40;11;10,5,20,4;0,0,160,1\x1b\\"
        );
        assert_eq!(remove("ab"), "\x1b]777;pepper;remove;ab\x1b\\");
    }

    #[test]
    fn holes_only_where_pepper_lists_them() {
        let features = |value: &str| holes_from(Some(value.into()));
        assert!(features("holes"));
        assert!(features("frames, holes"));
        assert!(!features(""));
        assert!(!features("holesome"));
        assert!(!holes_from(None), "an older Pepper");
    }

    #[test]
    fn holes_are_what_popups_cover_of_the_box() {
        let area = Rect::new(10, 5, 40, 20);
        // Cut to the box; one beside it is none.
        let popup = Rect::new(30, 10, 40, 6);
        assert_eq!(holes(area, &[popup]), [Rect::new(30, 10, 20, 6)]);
        assert_eq!(holes(area, &[Rect::new(0, 0, 10, 5)]), []);
        assert_eq!(holes(area, &[]), []);
        let many: Vec<Rect> = (0..5).map(|i| Rect::new(12 + 2 * i, 6 + i, 2, 1)).collect();
        assert_eq!(holes(area, &many[..4]), many[..4]);
        // More than Pepper takes: the two that take the least more of the
        // picture together are one, so the strip at the bottom of a full
        // screen and a popup stay as they are, not one box over most of it.
        let tab = Rect::new(0, 0, 160, 45);
        let over = [
            Rect::new(0, 41, 160, 4),
            Rect::new(50, 10, 60, 20),
            Rect::new(0, 0, 2, 1),
            Rect::new(158, 0, 2, 1),
            Rect::new(80, 35, 2, 1),
        ];
        let made = holes(tab, &over);
        assert_eq!(made.len(), MAX_HOLES, "{made:?}");
        assert!(
            made.contains(&over[0]) && made.contains(&over[1]),
            "{made:?}"
        );
        assert!(made.contains(&Rect::new(0, 0, 160, 1)), "{made:?}");
        assert!(
            over.iter()
                .all(|r| made.iter().any(|h| h.intersection(*r) == *r)),
            "every one in a hole: {made:?}"
        );
        // One inside another takes no hole of its own, so five can be
        // four with nothing merged.
        let inside = Rect::new(60, 15, 4, 2);
        let five = [over[0], over[1], inside, over[2], over[3]];
        assert_eq!(holes(tab, &five), [over[0], over[1], over[2], over[3]]);
        assert_eq!(holes(tab, &[over[1], over[1]]), [over[1]], "the same twice");
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
