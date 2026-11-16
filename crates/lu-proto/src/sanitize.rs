//! Output sanitising. Text from a peer is untrusted: it is made safe to display and capped in
//! length before it reaches a terminal or a log.

/// Makes a string from the network safe to draw: control characters, bidi overrides and
/// invisible format characters (zero-width marks, the byte-order mark) become `?`, so a peer's
/// name cannot move the cursor, recolour the screen, or reorder or hide the text around it.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if is_invisible(c) { '?' } else { c })
        .collect()
}

/// Keeps at most `max` characters of `text`, counting whole characters so a multi-byte
/// character is never split.
pub fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_and_line_breaks_are_neutralised() {
        assert_eq!(printable("ok\u{1b}[2J\nnext"), "ok?[2J?next");
        assert_eq!(printable("bell\u{7}"), "bell?");
        assert_eq!(printable("c1\u{9b}31m"), "c1?31m");
    }

    #[test]
    fn nul_and_other_control_characters_are_neutralised() {
        assert_eq!(printable("a\u{0}b"), "a?b");
        assert_eq!(printable("\r\n\t"), "???");
    }

    #[test]
    fn bidi_overrides_are_neutralised() {
        assert_eq!(printable("ab\u{202e}cd"), "ab?cd");
        assert_eq!(printable("x\u{2066}y"), "x?y");
    }

    #[test]
    fn line_and_paragraph_separators_are_neutralised() {
        assert_eq!(printable("a\u{2028}b"), "a?b");
        assert_eq!(printable("a\u{2029}b"), "a?b");
        assert_eq!(printable("\u{2028}\u{2029}"), "??");
    }

    #[test]
    fn zero_width_and_format_characters_are_neutralised() {
        assert_eq!(
            printable("a\u{200b}b\u{feff}c\u{2060}d\u{200f}e"),
            "a?b?c?d?e"
        );
    }

    #[test]
    fn ordinary_text_passes_through_unchanged() {
        assert_eq!(printable("peer-a 12 café ✓"), "peer-a 12 café ✓");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate_chars("hello", 3), "hel");
        assert_eq!(truncate_chars("hello", 0), "");
        assert_eq!(truncate_chars("hello", 99), "hello");
        assert_eq!(truncate_chars("héllo", 2), "hé");
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        let text = "aé✓𝄞";
        for max in 0..=text.chars().count() {
            let cut = truncate_chars(text, max);
            assert_eq!(cut.chars().count(), max);
            assert!(text.starts_with(&cut));
        }
    }
}
