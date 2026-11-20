//! Node labels. Lu never interprets a label; it only checks the wire shape and compares them
//! exactly, so a label may carry a version or a key=value pair.

use crate::limits::MAX_LABEL_BYTES;

/// The only label source marker an agent sends: an operator-declared probe published the label.
pub const LABEL_SOURCE_PROBED: &str = "probed";

/// Published by an agent after its harness crashes at launch. It is sent as both a node label and
/// a node taint so existing dispatch rules exclude the node for jobs that need a working harness;
/// a job may opt back in by tolerating it. A healthy run clears it.
pub const HARNESS_OK_FALSE: &str = "harness_ok=false";

/// Whether `value` may appear as a node label or a job requirement.
pub fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LABEL_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'='))
        && value.bytes().filter(|b| *b == b'=').count() <= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_labels_are_accepted() {
        for label in ["php8.3", "gpu", "region=eu", "os/linux", "a_b-c.d/e=f", "a"] {
            assert!(valid_label(label), "{label}");
        }
        assert!(valid_label(&"a".repeat(MAX_LABEL_BYTES)));
    }

    #[test]
    fn empty_overlong_and_bad_characters_are_refused() {
        for label in [
            String::new(),
            "a".repeat(MAX_LABEL_BYTES + 1),
            "has space".to_owned(),
            "café".to_owned(),
            "a=b=c".to_owned(),
            "semi;colon".to_owned(),
            "nul\0".to_owned(),
        ] {
            assert!(!valid_label(&label), "{label:?}");
        }
    }
}
