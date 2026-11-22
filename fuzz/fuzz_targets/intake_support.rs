//! Helpers shared by the two intake fuzz targets. This file is not itself a fuzz target.
//!
//! Only public API from `lu-controller` is used: rules are compiled with `Intake::from_config`
//! and evaluated with `Intake::map`. A private mode 0600 key file is created once per process so
//! config construction does not depend on the fuzz input.
//!
//! Each target compiles this module separately, so a helper used by only one of them is still
//! included in the other's build.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use lu_config::{IntakeCondition, IntakeConfig, IntakeRule, IntakeSource, IntakeValue};
use lu_controller::intake::{to_spec, IntakeJob};
use lu_proto::{MAX_CONTEXT_BYTES, MAX_IDEMPOTENCY_KEY_BYTES, MAX_ORIGIN_REF_BYTES};
use serde_json::{json, Map, Value};

/// Marker embedded in payload fields. Operator-authored strings are stripped of it so an
/// assertion failure can only be caused by payload text reaching `task`.
pub const SENTINEL: &str = "lu-fuzz-payload-sentinel";

/// `sanitize` caps values by character count, not byte count.
const MAX_FIELD_CHARS: usize = 256;

static SECRET: OnceLock<Option<PathBuf>> = OnceLock::new();

/// NUL-separated fields from the raw input, each bounded to a safe length.
pub struct Tokens {
    text: String,
    pos: usize,
}

impl Tokens {
    pub fn new(data: &[u8]) -> Self {
        Self {
            text: String::from_utf8_lossy(data).into_owned(),
            pos: 0,
        }
    }

    pub fn next(&mut self) -> &str {
        let start = self.pos;
        if start >= self.text.len() {
            return "";
        }
        let end = self.text[start..]
            .find('\0')
            .map(|offset| start + offset)
            .unwrap_or(self.text.len());
        self.pos = (end + 1).min(self.text.len());
        bound(&self.text[start..end], MAX_FIELD_CHARS)
    }

    pub fn byte(&mut self) -> u8 {
        self.next().as_bytes().first().copied().unwrap_or(0)
    }

    pub fn flag(&mut self) -> bool {
        self.byte() % 2 == 0
    }
}

fn bound(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((offset, _)) => &text[..offset],
        None => text,
    }
}

/// Operator-authored text: bounded, control-free and never containing the payload marker.
pub fn clean(text: &str) -> String {
    text.replace(SENTINEL, "")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_FIELD_CHARS)
        .collect()
}

/// A regex made only of literal characters, so every generated pattern compiles. Invalid
/// patterns are still exercised through `Intake::from_config`'s error path by other targets.
pub fn regex_token(text: &str) -> String {
    bound(text, MAX_FIELD_CHARS)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
        .collect()
}

pub fn value(text: String) -> IntakeValue {
    if text.starts_with('/') {
        IntakeValue::Pointer { pointer: text }
    } else {
        IntakeValue::Literal(text)
    }
}

pub fn secret_path() -> Option<&'static Path> {
    SECRET.get_or_init(create_secret).as_deref()
}

#[cfg(unix)]
fn create_secret() -> Option<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = std::env::temp_dir().join(format!("lu-fuzz-intake-{}.key", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    let mut file = options
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    file.write_all(b"lu-fuzz-hmac-key").ok()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok()?;
    Some(path)
}

#[cfg(not(unix))]
fn create_secret() -> Option<PathBuf> {
    None
}

pub fn config(secret: &Path, rules: Vec<IntakeRule>) -> IntakeConfig {
    IntakeConfig {
        sources: vec![IntakeSource {
            name: "fuzz".to_owned(),
            secret_file: secret.to_string_lossy().into_owned(),
            header: "X-Fuzz-Signature".to_owned(),
            max_body_bytes: 64 * 1024,
            rate_per_minute: 1000,
            dedupe_header: None,
            dedupe_ttl_secs: 60,
            allow_plain_http: false,
            rules,
        }],
    }
}

pub fn parsed(data: &[u8]) -> Option<Value> {
    serde_json::from_slice(data)
        .ok()
        .or_else(|| serde_json::from_str(&String::from_utf8_lossy(data)).ok())
}

/// A delivery body with known fields plus every extra object field supplied by the input.
pub fn delivery(data: &[u8]) -> Value {
    let mut fields = match parsed(data) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    fields.insert("action".to_owned(), json!(SENTINEL));
    fields.insert("actor".to_owned(), json!({ "login": SENTINEL }));
    fields.insert(
        "repository".to_owned(),
        json!({ "url": "https://example.invalid/r.git", "full_name": "org/r" }),
    );
    fields.insert(
        "pull_request".to_owned(),
        json!({
            "ref": "refs/heads/fuzz",
            "head": { "repo": { "full_name": "org/r" } }
        }),
    );
    fields.insert(
        "body".to_owned(),
        json!(format!("{SENTINEL}\n\u{1}\r\u{7}")),
    );
    fields.insert("title".to_owned(), json!(SENTINEL));
    fields.insert("delivery".to_owned(), json!({ "id": "d-fuzz" }));
    // Long ASCII value used to drive the context cap through its truncation branch. ASCII keeps
    // the byte-index cut on a character boundary.
    fields.insert("long".to_owned(), json!("a".repeat(512)));
    Value::Object(fields)
}

pub fn condition(tokens: &mut Tokens) -> IntakeCondition {
    let pointer = clean(&tokens.next());
    let (equals, in_values, prefix, regex, exists) = match tokens.byte() % 6 {
        0 => (Some(clean(&tokens.next())), Vec::new(), None, None, None),
        1 => (
            None,
            vec![clean(&tokens.next()), clean(&tokens.next())],
            None,
            None,
            None,
        ),
        2 => (None, Vec::new(), Some(clean(&tokens.next())), None, None),
        3 => (None, Vec::new(), None, Some(regex_token(&tokens.next())), None),
        4 => (None, Vec::new(), None, None, Some(tokens.flag())),
        _ => (None, Vec::new(), None, None, None),
    };
    IntakeCondition {
        pointer,
        equals,
        in_values,
        prefix,
        regex,
        exists,
    }
}

fn check_field(field: &str) {
    assert!(
        field.chars().count() <= MAX_FIELD_CHARS,
        "resolved field grew past the template cap"
    );
    assert!(
        field.chars().all(|c| !c.is_control()),
        "sanitised field kept a control character"
    );
}

pub fn check_context(context: &str) {
    assert!(
        context.len() <= MAX_CONTEXT_BYTES,
        "untrusted context exceeded its byte cap"
    );
    assert!(
        context.chars().all(|c| c == '\n' || !c.is_control()),
        "context kept a control character other than its structural newlines"
    );
}

pub fn check_job(job: &IntakeJob, source_name: &str) {
    check_field(&job.source.repo);
    check_field(&job.source.git_ref);
    if let Some(base) = &job.source.base {
        check_field(base);
    }
    if let Some(context) = &job.context {
        check_context(context);
    }
    if let Some(origin_ref) = &job.origin_ref {
        assert!(origin_ref.len() <= MAX_ORIGIN_REF_BYTES);
        assert!(origin_ref.chars().all(|c| !c.is_control()));
    }
    if let Some(key) = &job.key {
        assert!(key.len() <= MAX_IDEMPOTENCY_KEY_BYTES);
    }
    let spec = to_spec(job, source_name, Some("d-fuzz"));
    assert_eq!(spec.untrusted_context, job.context.is_some());
    assert_eq!(spec.task, job.task);
    assert_eq!(spec.source, job.source);
    assert_eq!(spec.origin.as_deref(), Some(source_name));
}
