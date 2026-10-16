//! Color themes: the same sixteen-color palettes as tuitube, tuigram and
//! tuimeta (`themes/` in the repository), mapped to what tuitok paints by
//! role, so drawing code never names a palette color directly.

use std::collections::BTreeMap;

use ratatui::style::Color;
use serde::Deserialize;

/// The theme used when the settings name none, or one that isn't built in.
pub const DEFAULT: &str = "mocha";

/// The built-in themes by name, lightest Catppuccin flavor first.
pub const BUILT_IN: [(&str, &str); 9] = [
    ("latte", include_str!("../themes/latte.toml")),
    ("frappe", include_str!("../themes/frappe.toml")),
    ("macchiato", include_str!("../themes/macchiato.toml")),
    ("mocha", include_str!("../themes/mocha.toml")),
    ("tokyonight", include_str!("../themes/tokyonight.toml")),
    ("dracula", include_str!("../themes/dracula.toml")),
    ("gruvbox", include_str!("../themes/gruvbox.toml")),
    ("nord", include_str!("../themes/nord.toml")),
    ("rose-pine", include_str!("../themes/rose-pine.toml")),
];

#[derive(Deserialize)]
struct ThemeFile {
    #[serde(default)]
    palette: BTreeMap<String, String>,
}

/// What the UI paints, by role.
#[derive(Clone, Debug, PartialEq)]
pub struct Colors {
    /// The window's background.
    pub bg: Color,
    /// Panels, and the video's box before a picture is in it.
    pub panel: Color,
    /// Borders of what doesn't have the keys.
    pub border: Color,
    /// Key hints, placeholders, separators.
    pub dim: Color,
    /// Authors, captions, counts.
    pub subtext: Color,
    pub text: Color,
    /// Errors, and what can't work here.
    pub red: Color,
    pub ok: Color,
    /// Keys, titles, what has the keys.
    pub accent: Color,
}

fn hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

impl Colors {
    /// The built-in theme called `id`, else the default one.
    pub fn named(id: &str) -> Self {
        let text = BUILT_IN
            .iter()
            .find(|(name, _)| *name == id)
            .or_else(|| BUILT_IN.iter().find(|(name, _)| *name == DEFAULT))
            .map(|(_, text)| *text)
            .expect("the default theme is built in");
        Self::parse(text).expect("built-in themes parse")
    }

    fn parse(text: &str) -> Option<Self> {
        let file: ThemeFile = toml::from_str(text).ok()?;
        let p = |name: &str| file.palette.get(name).and_then(|c| hex(c));
        Some(Self {
            bg: p("bg")?,
            panel: p("bg_alt")?,
            border: p("overlay")?,
            dim: p("comment")?,
            subtext: p("subtext")?,
            text: p("fg")?,
            red: p("red")?,
            ok: p("green")?,
            accent: p("accent")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_theme_has_the_whole_palette() {
        for (id, text) in BUILT_IN {
            assert!(Colors::parse(text).is_some(), "{id}");
        }
        assert_eq!(Colors::named("nope"), Colors::named(DEFAULT));
    }
}
