//! Controller-side redaction of bytes before an artifact is stored.
//!
//! Rules are deliberately provider-agnostic: built-in names describe the *shape* of a secret, not
//! who issued it. Every match becomes `[scrubbed:<rule>]`; operator rules supply their own name.
//!
//! Patches are handled line by line. Hunk headers are never touched, the diff line markers (` `,
//! `+`, `-`, `\`) and line terminators are preserved, and no replacement contains a newline. A
//! scrubbed patch may therefore still fail to apply (its content changed), which is reported as
//! `patch_scrubbed` in the job view so `lu collect` can require an explicit opt-in.

use lu_config::{ArtifactsConfig, BUILTIN_SCRUB_RULES};
use regex::bytes::Regex;

/// The tag prefix written in place of every scrubbed match.
pub const SCRUB_TAG_PREFIX: &[u8] = b"[scrubbed:";
/// End of a scrub tag.
pub const SCRUB_TAG_SUFFIX: &[u8] = b"]";

/// One compiled pattern and the fixed text that replaces a whole match.
#[derive(Debug, Clone)]
struct Pattern {
    name: String,
    re: Regex,
    replacement: Vec<u8>,
    /// Safe to apply to a single patch line (never consumes a line terminator).
    patch_safe: bool,
    /// Safe to apply to a whole log or report (may span lines).
    whole_safe: bool,
}

/// A configured scrubber. Cloning reuses the compiled automata (regexes are cheaply cloneable).
#[derive(Debug, Clone)]
pub struct Scrubber {
    enabled: bool,
    scrub_patch: bool,
    patterns: Vec<Pattern>,
}

/// High-confidence secret hits found in added patch lines when patch scrubbing is off.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PatchSecretHits {
    /// Number of matching added lines/spans.
    pub count: u64,
    /// Rule names that matched, deduplicated in first-seen order.
    pub rules: Vec<String>,
}

impl PatchSecretHits {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Result of scrubbing one artifact (or one log chunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubOutcome {
    /// Bytes to store. Equal to the input when scrubbing is off or nothing matched.
    pub data: Vec<u8>,
    /// Number of replaced matches.
    pub hits: u64,
    /// True when a `diff.patch` was changed.
    pub patch_scrubbed: bool,
    /// High-confidence secret hits in added patch lines. Only set when patch scrubbing is off.
    pub patch_secret_hits: PatchSecretHits,
}

/// Used only for the multi-line PEM block in whole artifacts.
const PRIVATE_KEY_BLOCK: &str =
    r"(?s)-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----.*?-----END (?:[A-Z0-9 ]+ )?PRIVATE KEY-----";
/// Marker lines of a PEM block, used in patches where lines are scrubbed independently.
const PRIVATE_KEY_MARKER: &str =
    r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----|-----END (?:[A-Z0-9 ]+ )?PRIVATE KEY-----";
const PRIVATE_KEY_BEGIN: &str = r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----";
const PRIVATE_KEY_END: &str = r"-----END (?:[A-Z0-9 ]+ )?PRIVATE KEY-----";

/// `Authorization: Bearer <value>` — the value must be at least 12 characters.
const BEARER: &str =
    r"(?i-u)(^|[^A-Za-z0-9_-])(authorization[ \t]*:[ \t]*bearer[ \t]+)([A-Za-z0-9._~+/=-]{12,})";
/// Well-known token shapes. Long random suffixes keep false positives rare; no issuer names are
/// used as rule names.
const TOKEN_PREFIX: &str = r"\b(?:ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{20,}|AKIA[A-Z0-9]{16})\b";
/// Three base64url segments with JSON-looking first two segments.
const JWT: &str = r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b";
/// Bare `password=`, `token=`, etc. values of at least 12 characters. Identifiers with quotes are
/// handled so `report.json` gets the same treatment as a shell-style assignment.
const ASSIGNMENT_BARE: &str = r#"(?i-u)((?:"(?:password|passwd|token|secret)"|\b(?:password|passwd|token|secret)\b)[ \t]*[:=][ \t]*)([^\s"'\r\n]{12,})"#;
const ASSIGNMENT_DOUBLE: &str = r#"(?i-u)((?:"(?:password|passwd|token|secret)"|\b(?:password|passwd|token|secret)\b)[ \t]*[:=][ \t]*")([^"\r\n]{12,})(")"#;
const ASSIGNMENT_SINGLE: &str = r#"(?i-u)((?:"(?:password|passwd|token|secret)"|\b(?:password|passwd|token|secret)\b)[ \t]*[:=][ \t]*')([^'\r\n]{12,})(')"#;

fn tag(rule: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(SCRUB_TAG_PREFIX.len() + rule.len() + SCRUB_TAG_SUFFIX.len());
    out.extend_from_slice(SCRUB_TAG_PREFIX);
    out.extend_from_slice(rule.as_bytes());
    out.extend_from_slice(SCRUB_TAG_SUFFIX);
    out
}

fn pattern(
    name: &str,
    source: &str,
    replacement: Vec<u8>,
    patch_safe: bool,
    whole_safe: bool,
) -> Pattern {
    let re = Regex::new(source)
        .unwrap_or_else(|e| panic!("built-in scrub pattern must compile: {source}: {e}"));
    Pattern {
        name: name.to_owned(),
        re,
        replacement,
        patch_safe,
        whole_safe,
    }
}

fn high_confidence_rule(name: &str) -> bool {
    matches!(
        name,
        "private_key" | "bearer_token" | "token_prefix" | "jwt"
    )
}

impl Default for Scrubber {
    fn default() -> Self {
        Self::builtins()
    }
}

impl Scrubber {
    /// False when the operator disabled scrubbing with `[artifacts] scrub = false`.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether diff patches are scrubbed in place. When false, patches are stored unchanged and
    /// inspected for high-confidence secrets instead.
    pub fn scrub_patch_enabled(&self) -> bool {
        self.enabled && self.scrub_patch
    }

    /// Scrubs a stored patch for display. `[artifacts] scrub_patch` controls whether uploads are
    /// changed on disk; a reader still gets patch-safe redaction so the `scrubbed` flag is honest.
    pub fn scrub_patch_for_read(&self, input: &[u8]) -> ScrubOutcome {
        if !self.enabled {
            return ScrubOutcome {
                data: input.to_vec(),
                hits: 0,
                patch_scrubbed: false,
                patch_secret_hits: PatchSecretHits::default(),
            };
        }
        self.scrub_patch(input)
    }

    /// Built-in rules only, with scrubbing enabled and patch scrubbing off (default).
    fn builtins() -> Self {
        let patterns = vec![
            pattern(
                "private_key",
                PRIVATE_KEY_BLOCK,
                tag("private_key"),
                false,
                true,
            ),
            pattern(
                "private_key",
                PRIVATE_KEY_MARKER,
                tag("private_key"),
                true,
                false,
            ),
            pattern(
                "bearer_token",
                BEARER,
                b"${1}${2}[scrubbed:bearer_token]".to_vec(),
                true,
                true,
            ),
            pattern(
                "token_prefix",
                TOKEN_PREFIX,
                tag("token_prefix"),
                true,
                true,
            ),
            pattern("jwt", JWT, tag("jwt"), true, true),
            pattern(
                "assignment",
                ASSIGNMENT_BARE,
                b"${1}[scrubbed:assignment]".to_vec(),
                true,
                true,
            ),
            pattern(
                "assignment",
                ASSIGNMENT_DOUBLE,
                b"${1}[scrubbed:assignment]${3}".to_vec(),
                true,
                true,
            ),
            pattern(
                "assignment",
                ASSIGNMENT_SINGLE,
                b"${1}[scrubbed:assignment]${3}".to_vec(),
                true,
                true,
            ),
        ];
        Self {
            enabled: true,
            scrub_patch: false,
            patterns,
        }
    }

    /// Builds a scrubber from `[artifacts]`. An absent table means enabled with built-in rules.
    /// Operator regexes are compiled here so startup and `lu check` reject bad patterns.
    pub fn from_config(config: Option<&ArtifactsConfig>) -> Result<Self, String> {
        let Some(config) = config else {
            return Ok(Self::builtins());
        };
        if !config.scrub {
            return Ok(Self {
                enabled: false,
                scrub_patch: false,
                patterns: Vec::new(),
            });
        }
        let mut this = Self::builtins();
        this.scrub_patch = config.scrub_patch;
        for rule in &config.extra_rules {
            let re = Regex::new(&rule.pattern).map_err(|e| {
                format!(
                    "[artifacts.extra_rules] {}: invalid pattern: {e}",
                    rule.name
                )
            })?;
            this.patterns.push(Pattern {
                name: rule.name.clone(),
                re,
                replacement: tag(&rule.name),
                patch_safe: true,
                whole_safe: true,
            });
        }
        Ok(this)
    }

    /// Validates config regexes without keeping the compiled scrubber.
    pub fn validate_config(config: Option<&ArtifactsConfig>) -> Result<(), String> {
        Self::from_config(config).map(|_| ())
    }

    /// Whether any scrubbing is configured.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Names of the built-in rules, for docs and diagnostics.
    pub fn builtin_rule_names() -> &'static [&'static str] {
        &BUILTIN_SCRUB_RULES
    }

    /// Scrubs one stored artifact. `patch` selects line-preserving mode for `diff.patch`.
    pub fn scrub(&self, input: &[u8], patch: bool) -> ScrubOutcome {
        if !self.enabled {
            return ScrubOutcome {
                data: input.to_vec(),
                hits: 0,
                patch_scrubbed: false,
                patch_secret_hits: PatchSecretHits::default(),
            };
        }
        if patch {
            if self.scrub_patch {
                self.scrub_patch(input)
            } else {
                ScrubOutcome {
                    data: input.to_vec(),
                    hits: 0,
                    patch_scrubbed: false,
                    patch_secret_hits: self.inspect_patch(input),
                }
            }
        } else {
            let (data, hits) = self.apply(input, |p| p.whole_safe);
            ScrubOutcome {
                data,
                hits,
                patch_scrubbed: false,
                patch_secret_hits: PatchSecretHits::default(),
            }
        }
    }

    fn apply(&self, input: &[u8], accept: impl Fn(&Pattern) -> bool) -> (Vec<u8>, u64) {
        let mut data = input.to_vec();
        let mut hits = 0u64;
        for p in self.patterns.iter().filter(|p| accept(p)) {
            let found = p.re.find_iter(&data).count() as u64;
            if found == 0 {
                continue;
            }
            data =
                p.re.replace_all(&data, p.replacement.as_slice())
                    .into_owned();
            hits += found;
        }
        (data, hits)
    }

    /// Applies rules inside a patch line while keeping the marker and terminator. A PEM block is
    /// tracked across lines so its body is scrubbed too, without merging or dropping lines.
    fn scrub_patch(&self, input: &[u8]) -> ScrubOutcome {
        let begin = Regex::new(PRIVATE_KEY_BEGIN).expect("valid private key marker");
        let end = Regex::new(PRIVATE_KEY_END).expect("valid private key marker");
        let mut data = Vec::with_capacity(input.len());
        let mut hits = 0u64;
        let mut in_key = false;

        for raw in input.split_inclusive(|b| *b == b'\n') {
            if raw.starts_with(b"@@") {
                data.extend_from_slice(raw);
                continue;
            }
            let (_, body_end) = if raw.ends_with(b"\r\n") {
                (2, raw.len() - 2)
            } else if raw.ends_with(b"\n") {
                (1, raw.len() - 1)
            } else {
                (0, raw.len())
            };
            let line = &raw[..body_end];
            // A diff line marker is metadata, never part of the content an operator wrote.
            let marker_len = usize::from(matches!(line.first(), Some(b' ' | b'+' | b'-' | b'\\')));
            let content = &line[marker_len..];
            data.extend_from_slice(&line[..marker_len]);

            if in_key {
                if content != tag("private_key").as_slice() {
                    hits += 1;
                }
                data.extend_from_slice(&tag("private_key"));
                if end.is_match(content) {
                    in_key = false;
                }
            } else {
                let (scrubbed, found) = self.apply(content, |p| p.patch_safe);
                hits += found;
                data.extend_from_slice(&scrubbed);
                if begin.is_match(content) && !end.is_match(content) {
                    in_key = true;
                }
            }
            data.extend_from_slice(&raw[body_end..]);
        }
        ScrubOutcome {
            data,
            hits,
            patch_scrubbed: hits > 0,
            patch_secret_hits: PatchSecretHits::default(),
        }
    }

    /// Inspects added patch lines for high-confidence secrets without changing bytes.
    fn inspect_patch(&self, input: &[u8]) -> PatchSecretHits {
        let begin = Regex::new(PRIVATE_KEY_BEGIN).expect("valid private key marker");
        let end = Regex::new(PRIVATE_KEY_END).expect("valid private key marker");
        let mut hits = 0u64;
        let mut rules: Vec<String> = Vec::new();
        let mut in_key = false;

        for raw in input.split_inclusive(|b| *b == b'\n') {
            let (_, body_end) = if raw.ends_with(b"\r\n") {
                (2, raw.len() - 2)
            } else if raw.ends_with(b"\n") {
                (1, raw.len() - 1)
            } else {
                (0, raw.len())
            };
            let line = &raw[..body_end];
            let added = line.first() == Some(&b'+') && !line.starts_with(b"+++");
            if !added {
                if in_key && line.starts_with(b"@@") {
                    in_key = false;
                }
                continue;
            }
            let content = &line[1..];

            if in_key {
                hits += 1;
                if !rules.iter().any(|r| r == "private_key") {
                    rules.push("private_key".to_owned());
                }
                if end.is_match(content) {
                    in_key = false;
                }
                continue;
            }

            for p in self
                .patterns
                .iter()
                .filter(|p| p.patch_safe && high_confidence_rule(&p.name))
            {
                let found = p.re.find_iter(content).count() as u64;
                if found == 0 {
                    continue;
                }
                hits += found;
                if !rules.iter().any(|r| r == &p.name) {
                    rules.push(p.name.clone());
                }
            }
            if begin.is_match(content) && !end.is_match(content) {
                // Marker line was already counted by the private_key marker pattern above.
                in_key = true;
            }
        }

        PatchSecretHits { count: hits, rules }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lu_config::{ArtifactsConfig, ScrubRuleConfig};

    fn scrub(text: &str) -> ScrubOutcome {
        Scrubber::default().scrub(text.as_bytes(), false)
    }

    fn scrub_patch(text: &str) -> ScrubOutcome {
        Scrubber::from_config(Some(&ArtifactsConfig {
            scrub: true,
            scrub_patch: true,
            extra_rules: Vec::new(),
        }))
        .unwrap()
        .scrub(text.as_bytes(), true)
    }

    fn text(outcome: &ScrubOutcome) -> String {
        String::from_utf8(outcome.data.clone()).unwrap()
    }

    #[test]
    fn private_key_block_is_scrubbed_and_certificates_are_not() {
        let input = "before\n-----BEGIN PRIVATE KEY-----\nQUJD\n-----END PRIVATE KEY-----\nafter\n";
        let out = scrub(input);
        assert_eq!(text(&out), "before\n[scrubbed:private_key]\nafter\n");
        assert_eq!(out.hits, 1);

        let cert = "-----BEGIN CERTIFICATE-----\nQUJD\n-----END CERTIFICATE-----";
        let out = scrub(cert);
        assert_eq!(text(&out), cert);
        assert_eq!(out.hits, 0);
    }

    #[test]
    fn bearer_values_are_scrubbed_but_short_or_other_schemes_are_not() {
        let input = "Authorization: Bearer abcdefghijklmnop\nX-Authorization: Bearer abcdefghijklmnop\nauthorization: bearer short\nbearer abcdefghijklmnop\n";
        let out = scrub(input);
        let s = text(&out);
        assert!(
            s.contains("Authorization: Bearer [scrubbed:bearer_token]"),
            "{s}"
        );
        assert!(
            s.contains("X-Authorization: Bearer abcdefghijklmnop"),
            "{s}"
        );
        assert!(s.contains("authorization: bearer short"), "{s}");
        assert!(s.contains("bearer abcdefghijklmnop"), "{s}");
        assert_eq!(out.hits, 1);
    }

    #[test]
    fn known_token_shapes_are_scrubbed_and_short_lookalikes_are_not() {
        let tokens = [
            "ghp_abcdefghijklmnopqrstuvwxyz012345",
            "github_pat_abcdefghijklmnopqrstuvwxyz012345",
            "sk-abcdefghijklmnopqrstuvwxyz012345",
            "AKIAABCDEFGHIJKLMNOP",
        ];
        for token in tokens {
            let out = scrub(&format!("value={token}"));
            assert_eq!(text(&out), "value=[scrubbed:token_prefix]", "{token}");
            assert_eq!(out.hits, 1, "{token}");
        }
        for negative in [
            "ghp_short",
            "sk-short",
            "AKIAshort",
            "plain-token-value-1234",
        ] {
            let out = scrub(negative);
            assert_eq!(text(&out), negative, "{negative}");
        }
    }

    #[test]
    fn jwt_triples_are_scrubbed_and_partials_are_not() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = scrub(&format!("Bearer {jwt}"));
        assert_eq!(text(&out), "Bearer [scrubbed:jwt]");
        assert_eq!(out.hits, 1);
        for partial in [
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            "eyJa.eyJb.eyJc",
        ] {
            let out = scrub(partial);
            assert_eq!(text(&out), partial, "{partial}");
        }
    }

    #[test]
    fn assignments_scrub_only_long_values() {
        for (input, expected) in [
            ("password=abcdefghijkl", "password=[scrubbed:assignment]"),
            ("token: abcdefghijklmn", "token: [scrubbed:assignment]"),
            (
                "secret = 'abcdefghijklmn'",
                "secret = '[scrubbed:assignment]'",
            ),
            (
                "PASSWORD=\"abcdefghijklmn\"",
                "PASSWORD=\"[scrubbed:assignment]\"",
            ),
            ("passwd:abcdefghijklmno", "passwd:[scrubbed:assignment]"),
            (
                r#"{"password":"supersecretvalue"}"#,
                r#"{"password":"[scrubbed:assignment]"}"#,
            ),
            (
                r#"{"token": "abcdefghijklmn"}"#,
                r#"{"token": "[scrubbed:assignment]"}"#,
            ),
        ] {
            let out = scrub(input);
            assert_eq!(out.hits, 1, "{input}");
            assert_eq!(text(&out), expected);
        }
        for input in [
            "password=short",
            "the token was long enough here",
            "secret=",
            "token = 12345678901",
        ] {
            let out = scrub(input);
            assert_eq!(out.hits, 0, "{input}");
            assert_eq!(text(&out), input);
        }
        let out = scrub("password=\"abcdefghijklmn\"");
        assert_eq!(text(&out), "password=\"[scrubbed:assignment]\"");
    }

    #[test]
    fn operator_rules_are_applied_after_builtins_and_can_be_disabled() {
        let artifacts = ArtifactsConfig {
            scrub: true,
            scrub_patch: false,
            extra_rules: vec![ScrubRuleConfig {
                name: "internal_id".to_owned(),
                pattern: r"ACME-[A-Z0-9]{16}".to_owned(),
            }],
        };
        let scrubber = Scrubber::from_config(Some(&artifacts)).unwrap();
        let out = scrubber.scrub(b"id ACME-A1B2C3D4E5F6G7H8", false);
        assert_eq!(text(&out), "id [scrubbed:internal_id]");
        assert_eq!(out.hits, 1);
        // Built-ins still fire with extras configured.
        let out = scrubber.scrub(b"token=abcdefghijklmn", false);
        assert!(!out.data.starts_with(b"token=abcdefghijklmn"));

        let off = ArtifactsConfig {
            scrub: false,
            scrub_patch: false,
            extra_rules: vec![ScrubRuleConfig {
                name: "internal_id".to_owned(),
                pattern: r"ACME-[A-Z0-9]{16}".to_owned(),
            }],
        };
        let scrubber = Scrubber::from_config(Some(&off)).unwrap();
        assert!(!scrubber.is_enabled());
        let out = scrubber.scrub(b"token=abcdefghijklmn ACME-A1B2C3D4E5F6G7H8", false);
        assert_eq!(out.hits, 0);
        assert_eq!(out.data, b"token=abcdefghijklmn ACME-A1B2C3D4E5F6G7H8");
    }

    #[test]
    fn malformed_operator_pattern_is_rejected() {
        let artifacts = ArtifactsConfig {
            scrub: true,
            scrub_patch: false,
            extra_rules: vec![ScrubRuleConfig {
                name: "broken".to_owned(),
                pattern: "([".to_owned(),
            }],
        };
        let err = Scrubber::validate_config(Some(&artifacts)).unwrap_err();
        assert!(err.contains("broken"), "{err}");
        assert!(err.contains("invalid pattern"), "{err}");
    }

    #[test]
    fn patch_keeps_hunk_headers_line_count_and_markers() {
        let patch = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-old\n+password=abcdefghijkl\n context\n@@ -10 +10 @@\n-Authorization: Bearer abcdefghijklmnop\n+fine\n";
        let out = scrub_patch(patch);
        let s = text(&out);
        assert_eq!(s.lines().count(), patch.lines().count());
        for line in patch.lines().filter(|line| line.starts_with("@@")) {
            assert!(
                s.lines().any(|got| got == line),
                "hunk header changed: {line}"
            );
        }
        assert!(s.contains("+password=[scrubbed:assignment]"), "{s}");
        assert!(
            s.contains("-Authorization: Bearer [scrubbed:bearer_token]"),
            "{s}"
        );
        assert!(s.contains(" context"), "{s}");
        assert!(out.patch_scrubbed);
        assert_eq!(out.hits, 2);
    }

    #[test]
    fn patch_scrubs_private_key_body_without_merging_lines() {
        let patch = "@@ -1,5 +1,5 @@\n+-----BEGIN PRIVATE KEY-----\n+QUJDREVG\n+-----END PRIVATE KEY-----\n+unchanged\n";
        let out = scrub_patch(patch);
        let s = text(&out);
        assert_eq!(s.lines().count(), patch.lines().count());
        assert_eq!(
            s,
            "@@ -1,5 +1,5 @@\n+[scrubbed:private_key]\n+[scrubbed:private_key]\n+[scrubbed:private_key]\n+unchanged\n"
        );
        assert_eq!(out.hits, 3);
    }

    #[test]
    fn patch_handles_crlf_and_final_line_without_newline() {
        let patch = "@@ -1 +1 @@\r\n+token=abcdefghijklmnop\r\n+no newline";
        let out = scrub_patch(patch);
        assert_eq!(
            out.data,
            b"@@ -1 +1 @@\r\n+token=[scrubbed:assignment]\r\n+no newline"
        );
        assert_eq!(out.hits, 1);
    }

    #[test]
    fn unchanged_patch_is_not_flagged() {
        let patch = "@@ -1 +1 @@\n+ordinary line\n";
        let out = scrub_patch(patch);
        assert_eq!(out.data, patch.as_bytes());
        assert_eq!(out.hits, 0);
        assert!(!out.patch_scrubbed);
    }

    #[test]
    fn built_in_rule_names_are_agnostic() {
        for name in Scrubber::builtin_rule_names() {
            for provider in ["github", "openai", "aws", "gitlab"] {
                assert!(!name.contains(provider), "provider name in rule {name}");
            }
        }
    }

    fn pem_private_key_block() -> String {
        let begin = ["-----BEGIN ", "PRIVATE KEY-----"].concat();
        let end = ["-----END ", "PRIVATE KEY-----"].concat();
        format!("{begin}\nQUJDREVG\n{end}\n")
    }

    #[test]
    fn patch_scrub_disabled_stores_patch_unchanged_and_reports_hits() {
        let _key = pem_private_key_block();
        let patch = format!(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,4 @@\n+{key_line1}\n+QUJDREVG\n+{key_line3}\n context\n",
            key_line1 = ["-----BEGIN ", "PRIVATE KEY-----"].concat(),
            key_line3 = ["-----END ", "PRIVATE KEY-----"].concat(),
        );
        let out = Scrubber::default().scrub(patch.as_bytes(), true);
        assert_eq!(out.data, patch.as_bytes());
        assert_eq!(out.hits, 0);
        assert!(!out.patch_scrubbed);
        assert!(out.patch_secret_hits.count > 0);
        assert!(out
            .patch_secret_hits
            .rules
            .iter()
            .any(|r| r == "private_key"));
    }

    #[test]
    fn patch_scrub_disabled_ignores_context_and_generic_assignment() {
        let assignment = format!("{}={}", "password", "abcdefghijkl");
        let patch = format!(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n {assignment}\n+ordinary line\n"
        );
        let out = Scrubber::default().scrub(patch.as_bytes(), true);
        assert_eq!(out.data, patch.as_bytes());
        assert_eq!(out.patch_secret_hits.count, 0);
        assert!(out.patch_secret_hits.rules.is_empty());
        assert!(!out.patch_scrubbed);
    }

    #[test]
    fn patch_scrub_enabled_keeps_old_patch_scrubbing_behavior() {
        let assignment = format!("{}={}", "password", "abcdefghijkl");
        let patch = format!("@@ -1 +1 @@\n+{assignment}\n");
        let out = scrub_patch(&patch);
        assert!(out.patch_scrubbed);
        assert!(out.hits > 0);
        assert!(text(&out).contains("[scrubbed:assignment]"));
        assert_eq!(out.patch_secret_hits.count, 0);
    }

    #[test]
    fn logs_and_reports_still_scrubbed_when_patch_scrub_disabled() {
        let token = format!("{}{}", "ghp_", "abcdefghijklmnopqrstuvwxyz012345");
        let input = format!("value={token}");
        let out = Scrubber::default().scrub(input.as_bytes(), false);
        assert!(
            text(&out).contains("[scrubbed:token_prefix]"),
            "got: {}",
            text(&out)
        );
        assert!(out.hits > 0, "got: {}", text(&out));
    }
}
