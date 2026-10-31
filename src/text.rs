//! Terminal-safe text. Model replies, tool output, file contents, and names
//! from the outside world reach the terminal only after these functions
//! neutralize characters that could move the cursor, change colors, or make
//! text display differently from what it is.

use std::borrow::Cow;

/// Whether `character` must not reach the terminal as itself: C0 and C1
/// controls (which include ESC and the single-byte CSI) and the Unicode
/// bidirectional embedding, override, and isolate controls, which can make
/// displayed text read differently from its contents.
pub(crate) fn is_unsafe(character: char) -> bool {
    character.is_control() || matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// What happens to tabs in single-line text.
#[derive(Clone, Copy)]
pub(crate) enum Tabs {
    Keep,
    /// Fixed-width layouts cannot expand tabs, so each becomes one space.
    Space,
}

/// One line of text with every unsafe character, newlines included,
/// replaced by U+FFFD.
pub(crate) fn sanitize_line(text: &str, tabs: Tabs) -> String {
    text.chars()
        .map(|character| match (character, tabs) {
            ('\t', Tabs::Keep) => '\t',
            ('\t', Tabs::Space) => ' ',
            (character, _) if is_unsafe(character) => char::REPLACEMENT_CHARACTER,
            (character, _) => character,
        })
        .collect()
}

/// Multi-line text with newlines and tabs kept and every other unsafe
/// character replaced by U+FFFD. Borrows when nothing needs replacing.
pub(crate) fn sanitize_block(text: &str) -> Cow<'_, str> {
    let keep = |character: char| matches!(character, '\n' | '\t') || !is_unsafe(character);
    if text.chars().all(keep) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|character| {
                if keep(character) {
                    character
                } else {
                    char::REPLACEMENT_CHARACTER
                }
            })
            .collect(),
    )
}

/// `text` cut to at most `max_chars` characters, ending in `…` when cut.
pub(crate) fn ellipsize(text: &str, max_chars: usize) -> String {
    let Some((cut, _)) = text.char_indices().nth(max_chars) else {
        return text.to_string();
    };
    if max_chars <= 1 {
        return "…".chars().take(max_chars).collect();
    }
    let end = text
        .char_indices()
        .nth(max_chars - 1)
        .map_or(cut, |(index, _)| index);
    format!("{}…", &text[..end])
}

/// `value` serialized on one line for machine-readable output. `serde_json`
/// escapes ASCII controls; this also escapes the C1 controls and
/// bidirectional overrides above, which can only occur inside strings, where
/// a `\u` escape decodes to the same text. The line is safe to display.
pub(crate) fn json_line(value: &serde_json::Value) -> String {
    let needs_escape = |character: char| !character.is_ascii() && is_unsafe(character);
    let raw = value.to_string();
    if !raw.chars().any(needs_escape) {
        return raw;
    }
    let mut line = String::with_capacity(raw.len() + 16);
    for character in raw.chars() {
        if needs_escape(character) {
            // A `\u` escape names one UTF-16 unit, which holds every unsafe
            // character today (C1 controls and bidirectional marks).
            // Anything beyond U+FFFF would need a surrogate pair.
            debug_assert!(
                u32::from(character) <= 0xFFFF,
                "is_unsafe matched {character:?} outside the BMP"
            );
            line.push_str(&format!("\\u{:04x}", u32::from(character)));
        } else {
            line.push(character);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_escape_terminal_controls_but_keep_the_text() {
        let value = serde_json::json!({"text": "a\u{1b}[2Jb\u{202e}c\u{85}d\u{e9}\nz"});
        let line = json_line(&value);
        assert!(!line.chars().any(is_unsafe));
        assert!(line.contains('\u{e9}'), "ordinary non-ASCII stays readable");
        let decoded: serde_json::Value = serde_json::from_str(&line).expect("still valid JSON");
        assert_eq!(decoded, value);
    }

    #[test]
    fn controls_and_bidirectional_overrides_are_unsafe() {
        for character in [
            '\u{1b}', '\u{9b}', '\u{7}', '\n', '\u{202E}', '\u{2066}', '\u{2069}',
        ] {
            assert!(is_unsafe(character), "{:?}", character);
        }
        // Ordinary right-to-left text and the implicit direction marks stay.
        for character in ['a', 'é', 'ש', 'ع', '\u{200E}', '\u{200F}'] {
            assert!(!is_unsafe(character), "{:?}", character);
        }
    }

    #[test]
    fn lines_replace_unsafe_characters_and_follow_the_tab_policy() {
        assert_eq!(
            sanitize_line("a\tb\u{1b}[2Jc\u{202E}d\ne", Tabs::Space),
            "a b\u{fffd}[2Jc\u{fffd}d\u{fffd}e"
        );
        assert_eq!(sanitize_line("a\tb", Tabs::Keep), "a\tb");
    }

    #[test]
    fn blocks_keep_layout_and_borrow_clean_text() {
        assert!(matches!(
            sanitize_block("plain\ttext\nlines"),
            Cow::Borrowed(_)
        ));
        assert_eq!(
            sanitize_block("ok\n\u{1b}]52;c;bad\u{7}\u{2067}x"),
            "ok\n\u{fffd}]52;c;bad\u{fffd}\u{fffd}x"
        );
    }

    #[test]
    fn ellipsize_counts_characters_and_marks_the_cut() {
        assert_eq!(ellipsize("héllo", 5), "héllo");
        assert_eq!(ellipsize("héllo", 4), "hél…");
        assert_eq!(ellipsize("héllo", 1), "…");
        assert_eq!(ellipsize("héllo", 0), "");
    }
}
