//! The job submission. Every field is range-checked and `validate` names the field at fault.
//! [`JobSpec::canonical_json`] is stable for equal specs, so the store can compare payloads byte
//! for byte to tell a repeated idempotency key from a conflicting one.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{is_name_byte, JobId};
use crate::limits::{
    DEFAULT_TIMEOUT_SECS, MAX_AFTER_JOBS, MAX_GIT_REF_BYTES, MAX_IDEMPOTENCY_KEY_BYTES,
    MAX_IDLE_TIMEOUT_SECS, MAX_MODEL_BYTES, MAX_NAME_BYTES, MAX_REPO_BYTES, MAX_TASK_BYTES,
    MAX_TIMEOUT_SECS, MIN_TIMEOUT_SECS,
};

/// A field of [`JobSpec`] that failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpecError {
    field: &'static str,
    reason: String,
}

impl JobSpecError {
    /// The offending field name, as it appears on the wire.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// Why the field was refused.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for JobSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for JobSpecError {}

/// A job to run. `harness` defaults to `opencode` and `timeout_secs` to an hour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobSpec {
    /// A git URL or an absolute local path.
    pub repo: String,
    pub git_ref: String,
    /// An optional 40-hex commit the agent should use as the base instead of the ref's tip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The prompt text. Control characters are allowed; they are sanitised for display.
    pub task: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Jobs that must be `done` before this one may run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<JobId>,
    /// Whether dependency diffs are applied to this job's base tree.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub stack_dependencies: bool,
}

#[derive(Deserialize)]
struct RawJobSpec {
    repo: String,
    git_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_commit: Option<String>,
    task: String,
    #[serde(default = "default_harness")]
    harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<String>,
    #[serde(default)]
    after: Vec<JobId>,
    #[serde(default = "default_true")]
    stack_dependencies: bool,
}

impl TryFrom<RawJobSpec> for JobSpec {
    type Error = JobSpecError;

    fn try_from(raw: RawJobSpec) -> Result<Self, Self::Error> {
        let spec = JobSpec {
            repo: raw.repo,
            git_ref: raw.git_ref,
            base_commit: raw.base_commit,
            task: raw.task,
            harness: raw.harness,
            provider: raw.provider,
            model: raw.model,
            timeout_secs: raw.timeout_secs,
            idle_timeout_secs: raw.idle_timeout_secs,
            idempotency_key: raw.idempotency_key,
            after: raw.after,
            stack_dependencies: raw.stack_dependencies,
        };
        spec.validate()?;
        Ok(spec)
    }
}

impl<'de> Deserialize<'de> for JobSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawJobSpec::deserialize(deserializer)?;
        JobSpec::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl JobSpec {
    /// Checks every field, returning the first fault with the field it belongs to.
    pub fn validate(&self) -> Result<(), JobSpecError> {
        check_repo(&self.repo)?;
        check_len("git_ref", &self.git_ref, MAX_GIT_REF_BYTES)?;
        if let Some(commit) = &self.base_commit {
            check_commit_id(commit)?;
        }
        check_len("task", &self.task, MAX_TASK_BYTES)?;
        check_name("harness", &self.harness)?;
        if let Some(provider) = &self.provider {
            check_name("provider", provider)?;
        }
        if let Some(model) = &self.model {
            check_len("model", model, MAX_MODEL_BYTES)?;
        }
        check_timeout("timeout_secs", self.timeout_secs, MAX_TIMEOUT_SECS)?;
        if let Some(idle) = self.idle_timeout_secs {
            check_timeout("idle_timeout_secs", idle, MAX_IDLE_TIMEOUT_SECS)?;
        }
        if let Some(key) = &self.idempotency_key {
            validate_idempotency_key(key)?;
        }
        if self.after.len() > MAX_AFTER_JOBS {
            return Err(invalid(
                "after",
                format!(
                    "must name at most {MAX_AFTER_JOBS} jobs (got {})",
                    self.after.len()
                ),
            ));
        }
        for (i, id) in self.after.iter().enumerate() {
            if self.after[..i].contains(id) {
                return Err(invalid("after", format!("duplicate job {id}")));
            }
        }
        Ok(())
    }

    /// A stable JSON encoding: fields in declaration order, absent options omitted. Equal specs
    /// always produce the same bytes.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).expect("a JobSpec always serialises")
    }
}

fn default_harness() -> String {
    "opencode".to_owned()
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn invalid(field: &'static str, reason: impl Into<String>) -> JobSpecError {
    JobSpecError {
        field,
        reason: reason.into(),
    }
}

fn check_len(field: &'static str, value: &str, max: usize) -> Result<(), JobSpecError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.len() > max {
        return Err(invalid(
            field,
            format!("must be at most {max} bytes (got {})", value.len()),
        ));
    }
    Ok(())
}

fn check_repo(repo: &str) -> Result<(), JobSpecError> {
    check_len("repo", repo, MAX_REPO_BYTES)?;
    if repo.chars().any(char::is_control) {
        return Err(invalid("repo", "must not contain control characters"));
    }
    if !repo.starts_with('/') && !repo.contains("://") {
        return Err(invalid(
            "repo",
            "must be a git URL or an absolute local path",
        ));
    }
    Ok(())
}

fn check_commit_id(value: &str) -> Result<(), JobSpecError> {
    if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(
            "base_commit",
            "must be 40 hexadecimal characters".to_owned(),
        ));
    }
    Ok(())
}

fn check_name(field: &'static str, value: &str) -> Result<(), JobSpecError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.len() > MAX_NAME_BYTES {
        return Err(invalid(
            field,
            format!(
                "must be at most {MAX_NAME_BYTES} characters (got {})",
                value.len()
            ),
        ));
    }
    if value.bytes().any(|b| !is_name_byte(b)) {
        return Err(invalid(field, "must only use A-Z a-z 0-9 . _ -"));
    }
    Ok(())
}

/// Checks a submitter-chosen key, using the same rules as a job spec.
pub fn validate_idempotency_key(value: &str) -> Result<(), JobSpecError> {
    check_len("idempotency_key", value, MAX_IDEMPOTENCY_KEY_BYTES)?;
    if value.bytes().any(|b| !(is_name_byte(b) || b == b':')) {
        return Err(invalid(
            "idempotency_key",
            "must only use A-Z a-z 0-9 . _ - :",
        ));
    }
    Ok(())
}

fn check_timeout(field: &'static str, secs: u64, max: u64) -> Result<(), JobSpecError> {
    if !(MIN_TIMEOUT_SECS..=max).contains(&secs) {
        return Err(invalid(
            field,
            format!("must be {MIN_TIMEOUT_SECS} to {max} seconds (got {secs})"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> JobSpec {
        JobSpec {
            repo: "https://example.net/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            base_commit: None,
            task: "run the tests".to_owned(),
            harness: "opencode".to_owned(),
            provider: None,
            model: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            idle_timeout_secs: None,
            idempotency_key: None,
            after: Vec::new(),
            stack_dependencies: true,
        }
    }

    fn error_field(spec: &JobSpec) -> &'static str {
        spec.validate().unwrap_err().field()
    }

    #[test]
    fn a_well_formed_spec_validates() {
        assert!(sample().validate().is_ok());
        let mut spec = sample();
        spec.repo = "/srv/git/repo".to_owned();
        spec.provider = Some("peer-a".to_owned());
        spec.model = Some("big-pickle".to_owned());
        spec.idle_timeout_secs = Some(30);
        spec.idempotency_key = Some("ticket:1234".to_owned());
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn every_field_reports_its_own_error() {
        let mut spec = sample();
        spec.repo = String::new();
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.repo = "relative/path".to_owned();
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.repo = format!("https://example.net/{}", "a".repeat(MAX_REPO_BYTES));
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.repo = "https://example.net/a\u{0}b".to_owned();
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.git_ref = String::new();
        assert_eq!(error_field(&spec), "git_ref");

        spec = sample();
        spec.git_ref = "a".repeat(MAX_GIT_REF_BYTES + 1);
        assert_eq!(error_field(&spec), "git_ref");

        spec = sample();
        spec.base_commit = Some("abc".to_owned());
        assert_eq!(error_field(&spec), "base_commit");
        spec = sample();
        spec.base_commit = Some(format!("{}z", "a".repeat(39)));
        assert_eq!(error_field(&spec), "base_commit");

        spec = sample();
        spec.task = String::new();
        assert_eq!(error_field(&spec), "task");

        spec = sample();
        spec.task = "a".repeat(MAX_TASK_BYTES + 1);
        assert_eq!(error_field(&spec), "task");

        spec = sample();
        spec.harness = "bad harness".to_owned();
        assert_eq!(error_field(&spec), "harness");

        spec = sample();
        spec.harness = "a".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(error_field(&spec), "harness");

        spec = sample();
        spec.provider = Some("bad/provider".to_owned());
        assert_eq!(error_field(&spec), "provider");

        spec = sample();
        spec.model = Some("m".repeat(MAX_MODEL_BYTES + 1));
        assert_eq!(error_field(&spec), "model");

        spec = sample();
        spec.model = Some(String::new());
        assert_eq!(error_field(&spec), "model");

        spec = sample();
        spec.timeout_secs = 0;
        assert_eq!(error_field(&spec), "timeout_secs");

        spec = sample();
        spec.timeout_secs = MAX_TIMEOUT_SECS + 1;
        assert_eq!(error_field(&spec), "timeout_secs");

        spec = sample();
        spec.idle_timeout_secs = Some(0);
        assert_eq!(error_field(&spec), "idle_timeout_secs");

        spec = sample();
        spec.idempotency_key = Some("has space".to_owned());
        assert_eq!(error_field(&spec), "idempotency_key");

        spec = sample();
        spec.idempotency_key = Some("k".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1));
        assert_eq!(error_field(&spec), "idempotency_key");

        spec = sample();
        spec.after = (0..=MAX_AFTER_JOBS).map(|_| JobId::new()).collect();
        assert_eq!(error_field(&spec), "after");

        spec = sample();
        let id = JobId::new();
        spec.after = vec![id.clone(), id];
        assert_eq!(error_field(&spec), "after");
    }

    #[test]
    fn a_validation_error_names_the_field_in_its_message() {
        let mut spec = sample();
        spec.git_ref = String::new();
        let error = spec.validate().unwrap_err();
        assert_eq!(error.field(), "git_ref");
        assert!(error.to_string().starts_with("git_ref:"), "{error}");
    }

    #[test]
    fn defaults_fill_in_for_harness_and_timeout() {
        let json = r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.harness, "opencode");
        assert_eq!(spec.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(spec.provider, None);
        assert_eq!(spec.base_commit, None);
        assert!(spec.after.is_empty());
        assert!(spec.stack_dependencies);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn an_old_style_spec_without_base_commit_still_deserialises() {
        let json = r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.base_commit, None);
        assert!(!spec.canonical_json().contains("base_commit"));
    }

    #[test]
    fn a_pinned_spec_serialises_and_round_trips_base_commit() {
        let mut spec = sample();
        spec.base_commit = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
        let json = spec.canonical_json();
        assert!(json.contains("base_commit"));
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let json =
            r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go","surprise":1}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn canonical_json_is_stable_across_field_order_and_round_trips() {
        let spec = sample();
        let json = spec.canonical_json();
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);

        let reordered = format!(
            r#"{{"task":"{}","timeout_secs":{},"harness":"{}","git_ref":"{}","repo":"{}"}}"#,
            spec.task, spec.timeout_secs, spec.harness, spec.git_ref, spec.repo
        );
        let parsed: JobSpec = serde_json::from_str(&reordered).unwrap();
        assert_eq!(parsed.canonical_json(), json);
    }

    #[test]
    fn canonical_json_keeps_declaration_order_and_omits_absent_options() {
        let mut spec = sample();
        spec.provider = Some("peer-a".to_owned());
        let json = spec.canonical_json();
        let positions: Vec<usize> = [
            "repo",
            "git_ref",
            "task",
            "harness",
            "provider",
            "timeout_secs",
        ]
        .iter()
        .map(|key| json.find(&format!("\"{key}\"")).unwrap())
        .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{json}");
        assert!(!json.contains("model"), "{json}");
        assert!(!json.contains("idempotency_key"), "{json}");
    }

    #[test]
    fn an_optional_base_commit_round_trips_and_is_omitted_when_absent() {
        let mut spec = sample();
        assert!(!spec.canonical_json().contains("base_commit"));
        spec.base_commit = Some("a".repeat(40));
        let json = spec.canonical_json();
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn dependencies_round_trip_and_are_omitted_when_empty() {
        let mut spec = sample();
        assert!(!spec.canonical_json().contains("after"));
        spec.after = vec![JobId::new(), JobId::new()];
        let json = spec.canonical_json();
        assert!(json.contains("\"after\""));
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);
    }

    #[test]
    fn stacking_defaults_true_and_is_omitted_only_when_true() {
        let mut spec = sample();
        assert!(spec.stack_dependencies);
        assert!(!spec.canonical_json().contains("stack_dependencies"));

        spec.stack_dependencies = false;
        let json = spec.canonical_json();
        assert!(json.contains("\"stack_dependencies\":false"), "{json}");
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert!(!parsed.stack_dependencies);
        assert_eq!(parsed, spec);
    }

    #[test]
    fn hostile_task_text_still_round_trips_and_is_left_for_display_to_sanitise() {
        let mut spec = sample();
        spec.task = "nul\u{0} esc\u{1b}[31m bidi\u{202e}".to_owned();
        assert!(spec.validate().is_ok());
        let parsed: JobSpec = serde_json::from_str(&spec.canonical_json()).unwrap();
        assert_eq!(parsed.task, spec.task);
    }
}
