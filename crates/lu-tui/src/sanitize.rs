//! One sanitiser for everything from the network: the protocol crate's, so the dashboard and the
//! CLI never disagree about what is safe to draw.

pub use lu_proto::printable;

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn line_and_paragraph_separators_cannot_reflow_the_screen() {
        assert_eq!(printable("a\u{2028}b\u{2029}c"), "a?b?c");
    }

    #[test]
    fn escapes_and_bidi_overrides_are_neutralised() {
        assert_eq!(printable("x\u{1b}[31m"), "x?[31m");
        assert_eq!(printable("a\u{202e}b"), "a?b");
    }
}
