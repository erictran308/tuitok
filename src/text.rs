//! Text from other people, made safe to show and copy (tuitube's `text.rs`).

/// Characters left out of text from TikTok:
/// - control characters other than line breaks and tabs. The screen drops
///   them anyway, but copied text keeps them, and a hidden `\r` or escape
///   can run a command when pasted into a shell or vim.
/// - bidi overrides and isolates, which can make text read backwards:
///   "invoice<U+202E>fdp.exe" shows as `invoiceexe.pdf` in some terminals,
///   and the bidi marks.
/// - invisible characters that terminals and the layout disagree on: the
///   layout counts them as no columns, while some terminals move the cursor
///   on, which pushes the rest of the row out of place. These are the Hangul
///   fillers of "invisible" names, the soft hyphen, zero-width spaces and
///   the like. Joiners and variation selectors stay, since emoji need them.
/// - the line and paragraph separators (U+2028, U+2029).
/// - the private character kitty uses to place images, so text can't pose
///   as one.
pub fn is_hidden(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(
            c,
            '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{061C}'
                | '\u{00AD}'
                | '\u{034F}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{2800}'
                | '\u{200B}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{2060}'..='\u{2064}'
                | '\u{206A}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{10EEEE}'
        )
}

/// `text` without the characters [`is_hidden`] leaves out.
pub fn clean(text: &str) -> String {
    text.chars().filter(|&c| !is_hidden(c)).collect()
}

/// The first `n` characters of `text`.
pub fn first_chars(text: &str, n: usize) -> &str {
    text.char_indices().nth(n).map_or(text, |(i, _)| &text[..i])
}

/// `text` cleaned, every run of whitespace (line breaks too) one space, at
/// most `max` characters: names, a song, an error.
pub fn one_line(text: &str, max: usize) -> String {
    let joined = clean(text).split_whitespace().collect::<Vec<_>>().join(" ");
    first_chars(&joined, max).to_owned()
}

/// `text` cleaned, each line's whitespace collapsed, blank lines kept as
/// one and none at either end, at most `max` characters: a caption.
pub fn lines(text: &str, max: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in clean(text).lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() && out.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|last| last.is_empty()) {
        out.pop();
    }
    first_chars(&out.join("\n"), max).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_bidi_overrides_are_dropped_but_line_breaks_and_tabs_stay() {
        assert_eq!(clean("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
        assert_eq!(clean("a\u{1b}[2Jb\rc\u{7}d\u{9b}e"), "a[2Jbcde");
        assert_eq!(clean("one\ntwo\tthree"), "one\ntwo\tthree");
        assert_eq!(clean("legit\u{2028}spoof\u{2029}more"), "legitspoofmore");
    }

    #[test]
    fn invisible_fillers_go_but_emoji_keep_their_joiners_and_selectors() {
        let filler = format!("x{}SPOOF", "\u{3164}".repeat(40));
        assert_eq!(clean(&filler), "xSPOOF");
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(clean(family), family);
        assert_eq!(clean("❤\u{FE0F}"), "❤\u{FE0F}");
    }

    #[test]
    fn one_line_and_lines() {
        assert_eq!(one_line("  a\n\tb  \u{1b}c ", 10), "a b c");
        assert_eq!(one_line("héllo world", 3), "hél");
        assert_eq!(
            lines("\n\none  two\n\n\n\nthree\n\n", 100),
            "one two\n\nthree"
        );
        assert_eq!(first_chars("hi", 5), "hi");
    }
}
