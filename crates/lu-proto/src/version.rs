//! Build-version parsing, ordering and display.
//!
//! A build version is the workspace SemVer: `major.minor.patch`, an optional pre-release such as
//! `-dev.1.gc08a5a0`, and optional `+` metadata. Release tags and the `x-lu-version` header add a
//! `v` or `lu/` prefix, which parsing ignores. Display keeps every part of the version, so a dev
//! build never loses its counter or commit hash.

use std::cmp::Ordering;
use std::fmt;

/// A parsed build version. Precedence is SemVer-like; metadata breaks ties so two builds with the
/// same pre-release but different hashes still compare deterministically.
#[derive(Debug, Clone)]
pub struct BuildVersion<'a> {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Option<&'a str>,
    metadata: Option<&'a str>,
    raw: &'a str,
}

/// A version string is not a SemVer build version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseVersionError;

impl fmt::Display for ParseVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid build version")
    }
}

impl std::error::Error for ParseVersionError {}

impl<'a> BuildVersion<'a> {
    /// Parse a bare version, a `v`-prefixed release tag or a `lu/`-prefixed HTTP header value.
    pub fn parse(input: &'a str) -> Result<Self, ParseVersionError> {
        let raw = input.strip_prefix("lu/").unwrap_or(input).trim();
        let raw = raw.strip_prefix('v').unwrap_or(raw);
        let (without_meta, metadata) = match raw.split_once('+') {
            Some((head, meta)) if valid_identifiers(meta) => (head, Some(meta)),
            Some(_) => return Err(ParseVersionError),
            None => (raw, None),
        };
        let (core, prerelease) = match without_meta.split_once('-') {
            Some((core, pre)) if valid_identifiers(pre) => (core, Some(pre)),
            Some(_) => return Err(ParseVersionError),
            None => (without_meta, None),
        };
        let mut parts = core.split('.');
        let major = parts
            .next()
            .and_then(|part| part.parse().ok())
            .ok_or(ParseVersionError)?;
        let minor = parts
            .next()
            .and_then(|part| part.parse().ok())
            .ok_or(ParseVersionError)?;
        let patch = parts
            .next()
            .and_then(|part| part.parse().ok())
            .ok_or(ParseVersionError)?;
        if parts.next().is_some() {
            return Err(ParseVersionError);
        }
        Ok(Self {
            major,
            minor,
            patch,
            prerelease,
            metadata,
            raw,
        })
    }

    /// The numeric `major.minor.patch` core.
    pub fn core(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }

    /// Whether this is an automatic dev build: `dev.N.g<hash>` or `dev.N+<hash>`.
    pub fn is_dev(&self) -> bool {
        self.prerelease
            .is_some_and(|pre| pre.split('.').next() == Some("dev"))
    }

    fn cmp_prerelease(&self, other: &Self) -> Ordering {
        match (self.prerelease, other.prerelease) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => compare_prerelease(left, right),
        }
    }
}

fn valid_identifiers(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Split into digit and non-digit runs. This keeps `dev.2` below `dev.10` and `rc2` below `rc10`.
fn prerelease_tokens(value: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut started_as_digit = false;
    for (index, byte) in value.bytes().enumerate() {
        if !byte.is_ascii_alphanumeric() {
            if let Some(first) = start.take() {
                tokens.push(&value[first..index]);
            }
            continue;
        }
        let is_digit = byte.is_ascii_digit();
        match start {
            None => {
                start = Some(index);
                started_as_digit = is_digit;
            }
            Some(first) if started_as_digit != is_digit => {
                tokens.push(&value[first..index]);
                start = Some(index);
                started_as_digit = is_digit;
            }
            Some(_) => {}
        }
    }
    if let Some(first) = start {
        tokens.push(&value[first..]);
    }
    tokens
}

fn compare_identifier(left: &str, right: &str) -> Ordering {
    let left_numeric = left.bytes().all(|byte| byte.is_ascii_digit());
    let right_numeric = right.bytes().all(|byte| byte.is_ascii_digit());
    match (left_numeric, right_numeric) {
        (true, true) => {
            let left = left.trim_start_matches('0');
            let right = right.trim_start_matches('0');
            left.len().cmp(&right.len()).then_with(|| left.cmp(right))
        }
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left.cmp(right),
    }
}

fn compare_prerelease(left: &str, right: &str) -> Ordering {
    let left = prerelease_tokens(left);
    let right = prerelease_tokens(right);
    for (a, b) in left.iter().zip(right.iter()) {
        let order = compare_identifier(a, b);
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

impl PartialEq for BuildVersion<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for BuildVersion<'_> {}

impl Ord for BuildVersion<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core()
            .cmp(&other.core())
            .then_with(|| self.cmp_prerelease(other))
            .then_with(|| self.metadata.cmp(&other.metadata))
    }
}

impl PartialOrd for BuildVersion<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for BuildVersion<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.raw)
    }
}

/// Compare an agent build with the value reported by the controller, ignoring the `lu/` header
/// prefix. Unknown or opaque strings still compare literally, so display never guesses.
pub fn same_build(left: &str, right: &str) -> bool {
    match (BuildVersion::parse(left), BuildVersion::parse(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "0.6.2-dev.1.gc08a5a0";
    const DEV_META: &str = "0.6.2-dev.1+gc08a5a0";

    #[test]
    fn parses_a_dev_build_without_losing_the_counter_or_hash() {
        let version = BuildVersion::parse(DEV).expect("fake dev version");
        assert_eq!(version.core(), (0, 6, 2));
        assert_eq!(version.prerelease, Some("dev.1.gc08a5a0"));
        assert!(version.is_dev());
        assert_eq!(version.to_string(), DEV);

        let with_meta = BuildVersion::parse(DEV_META).expect("fake metadata version");
        assert_eq!(with_meta.core(), (0, 6, 2));
        assert_eq!(with_meta.metadata, Some("gc08a5a0"));
        assert_eq!(with_meta.to_string(), DEV_META);
        assert!(!BuildVersion::parse("0.6.2").unwrap().is_dev());
    }

    #[test]
    fn orders_dev_builds_between_stable_releases() {
        let previous = BuildVersion::parse("0.6.1").unwrap();
        let dev = BuildVersion::parse(DEV).unwrap();
        let stable = BuildVersion::parse("0.6.2").unwrap();
        let next = BuildVersion::parse("0.6.3").unwrap();
        assert!(previous < dev);
        assert!(dev < stable);
        assert!(stable < next);
    }

    #[test]
    fn orders_dev_counters_numerically() {
        let low = BuildVersion::parse("0.6.2-dev.2.gc08a5a0").unwrap();
        let high = BuildVersion::parse("0.6.2-dev.10.gc08a5a0").unwrap();
        let stable = BuildVersion::parse("0.6.2").unwrap();
        assert!(low < high);
        assert!(high < stable);
    }

    #[test]
    fn display_keeps_prefixes_out_and_metadata_in() {
        assert_eq!(
            BuildVersion::parse("v0.6.2-dev.1.gc08a5a0")
                .unwrap()
                .to_string(),
            DEV
        );
        assert_eq!(
            BuildVersion::parse("lu/0.6.2-dev.1+gc08a5a0")
                .unwrap()
                .to_string(),
            DEV_META
        );
    }

    #[test]
    fn same_build_accepts_the_controller_header_prefix() {
        assert!(same_build(DEV, &format!("lu/{DEV}")));
        assert!(!same_build(DEV, "0.6.2-dev.2.gc08a5a0"));
        assert!(same_build("opaque", "opaque"));
    }

    #[test]
    fn equality_agrees_with_ordering() {
        let plain = BuildVersion::parse("0.6.2").unwrap();
        let padded = BuildVersion::parse("0.6.02").unwrap();
        assert_eq!(plain.cmp(&padded), Ordering::Equal);
        assert_eq!(plain, padded);
        assert!(same_build("v0.6.2", "lu/0.6.2"));
    }

    #[test]
    fn odd_input_never_panics() {
        for value in [
            "\u{e9}.1.2",
            "1.2.3-\u{e9}",
            "+",
            "-",
            "v",
            "lu/",
            "99999999999999999999.0.0",
            "1.2.3-0.0",
            "1.2.3--",
            "1.2.3+a+b",
            "1.2.3-a+b-c",
            "  ",
            "\0",
        ] {
            let _ = BuildVersion::parse(value);
        }
    }

    #[test]
    fn malformed_versions_return_errors_without_panicking() {
        for value in ["", "0.6", "0.6.x.1", "0.6.1-", "0.6.1+", "0.6.1-a..b"] {
            assert!(
                BuildVersion::parse(value).is_err(),
                "unexpectedly parsed {value}"
            );
        }
    }
}
