//! The job submission. Every field is range-checked and `validate` names the field at fault.
//! [`JobSpec::canonical_json`] is stable for equal specs, so the store can compare payloads byte
//! for byte to tell a repeated idempotency key from a conflicting one.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::{is_name_byte, JobId};
use crate::label::valid_label;
use crate::limits::{
    DEFAULT_TIMEOUT_SECS, MAX_AFTER_JOBS, MAX_COLLECT_INPUTS, MAX_FALLBACKS, MAX_GIT_REF_BYTES,
    MAX_IDEMPOTENCY_KEY_BYTES, MAX_IDLE_TIMEOUT_SECS, MAX_MODEL_BYTES, MAX_NAME_BYTES,
    MAX_REPO_BYTES, MAX_REQUIRES_LABELS, MAX_TASK_BYTES, MAX_TIMEOUT_SECS, MAX_TOLERATES,
    MIN_TIMEOUT_SECS,
};

/// The provider name that asks dispatch to choose a healthy provider from the node's offer.
pub const ANY_PROVIDER: &str = "any";

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

/// An alternative provider and model a job may run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// What a job stages from its dependencies before the harness starts.
///
/// `None` keeps the old behaviour: no `.lu/inputs` directory and no explicit stacking. An absent
/// `collect` on the wire falls back to `stack_dependencies`, so existing specs keep applying diffs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectMode {
    #[default]
    None,
    Files,
    Apply,
    Both,
}

/// The canonical spellings accepted by [`CollectMode::parse`], in help-text order.
pub const COLLECT_MODES: [&str; 4] = ["none", "files", "apply", "both"];

/// Alternate spellings accepted by [`CollectMode::parse`]. Each maps to a canonical mode.
pub const COLLECT_ALIASES: [(&str, CollectMode); 6] = [
    ("off", CollectMode::None),
    ("false", CollectMode::None),
    ("diffs", CollectMode::Files),
    ("diff", CollectMode::Files),
    ("patches", CollectMode::Files),
    ("patch", CollectMode::Files),
];

impl CollectMode {
    /// Parses a canonical or alias spelling, case-insensitively.
    pub fn parse(value: &str) -> Result<Self, CollectModeError> {
        let text = value.trim();
        for name in COLLECT_MODES {
            if text.eq_ignore_ascii_case(name) {
                return Ok(match name {
                    "none" => Self::None,
                    "files" => Self::Files,
                    "apply" => Self::Apply,
                    _ => Self::Both,
                });
            }
        }
        for (alias, mode) in COLLECT_ALIASES {
            if text.eq_ignore_ascii_case(alias) {
                return Ok(mode);
            }
        }
        Err(CollectModeError {
            value: value.to_owned(),
        })
    }

    /// The canonical wire spelling, also used in `JobSpec::canonical_json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Files => "files",
            Self::Apply => "apply",
            Self::Both => "both",
        }
    }

    /// Whether dependency artifacts are written under `.lu/inputs`.
    pub fn collects_files(self) -> bool {
        matches!(self, Self::Files | Self::Both)
    }

    /// Whether dependency diffs are applied to the job's tree.
    pub fn applies_diffs(self) -> bool {
        matches!(self, Self::Apply | Self::Both)
    }
}

/// What to do when an input patch does not apply cleanly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnConflict {
    /// Stop before the harness and end the job as `conflict`.
    #[default]
    Fail,
    /// Leave standard conflict markers in the affected files and continue with later inputs.
    Markers,
    /// Leave the patch unapplied and continue with later inputs.
    Skip,
}

/// The canonical spellings accepted by [`OnConflict::parse`], in help-text order.
pub const ON_CONFLICT_MODES: [&str; 3] = ["fail", "markers", "skip"];

impl OnConflict {
    /// Parses a canonical spelling, case-insensitively.
    pub fn parse(value: &str) -> Result<Self, OnConflictError> {
        let text = value.trim();
        for name in ON_CONFLICT_MODES {
            if text.eq_ignore_ascii_case(name) {
                return Ok(match name {
                    "fail" => Self::Fail,
                    "markers" => Self::Markers,
                    _ => Self::Skip,
                });
            }
        }
        Err(OnConflictError {
            value: value.to_owned(),
        })
    }

    /// The canonical wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fail => "fail",
            Self::Markers => "markers",
            Self::Skip => "skip",
        }
    }
}

impl FromStr for OnConflict {
    type Err = OnConflictError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for OnConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An `on_conflict` value that is not a canonical mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnConflictError {
    value: String,
}

impl OnConflictError {
    /// The rejected input.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for OnConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid on_conflict mode {:?}; expected one of {}",
            self.value,
            ON_CONFLICT_MODES.join(", ")
        )
    }
}

impl std::error::Error for OnConflictError {}

impl FromStr for CollectMode {
    type Err = CollectModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for CollectMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `collect` value that is neither a canonical mode nor an alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectModeError {
    value: String,
}

impl CollectModeError {
    /// The rejected input.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for CollectModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid collect mode {:?}; expected one of {} (aliases: {})",
            self.value,
            COLLECT_MODES.join(", "),
            COLLECT_ALIASES
                .iter()
                .map(|(alias, _)| *alias)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for CollectModeError {}

/// Resolves the effective mode: an explicit `collect` always wins over `stack_dependencies`.
pub fn effective_collect(mode: Option<CollectMode>, stack_dependencies: bool) -> CollectMode {
    mode.unwrap_or(if stack_dependencies {
        CollectMode::Apply
    } else {
        CollectMode::None
    })
}

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
    /// Which dependency artifacts to stage under `.lu/inputs`. Absent means
    /// [`effective_collect`] falls back to `stack_dependencies`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect: Option<CollectMode>,
    /// Include `failed` and `cancelled` dependencies as inputs when they stored a diff.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_failed: bool,
    /// How to handle an input patch that does not apply cleanly.
    #[serde(default, skip_serializing_if = "is_default_conflict")]
    pub on_conflict: OnConflict,
    /// Whether an empty `diff.patch` is expected, so no `no_changes` note is added.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_empty: bool,
    /// Labels a node must advertise before this job may run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Node taints this job tolerates. Every taint on a node must appear here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerates: Vec<String>,
    /// Alternatives to the primary provider and model, tried in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<Choice>,
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
    /// Kept as text so an invalid value is reported as a `collect` field error, with the same
    /// aliases the CLI and MCP accept.
    #[serde(default, alias = "collect_mode")]
    collect: Option<String>,
    #[serde(default)]
    include_failed: bool,
    /// Kept as text so an invalid value is reported as an `on_conflict` field error.
    #[serde(default)]
    on_conflict: Option<String>,
    #[serde(default)]
    allow_empty: bool,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    tolerates: Vec<String>,
    #[serde(default)]
    fallbacks: Vec<Choice>,
}

impl TryFrom<RawJobSpec> for JobSpec {
    type Error = JobSpecError;

    fn try_from(raw: RawJobSpec) -> Result<Self, Self::Error> {
        let collect = raw
            .collect
            .as_deref()
            .map(CollectMode::parse)
            .transpose()
            .map_err(|error| invalid("collect", error.to_string()))?;
        let on_conflict = raw
            .on_conflict
            .as_deref()
            .map(OnConflict::parse)
            .transpose()
            .map_err(|error| invalid("on_conflict", error.to_string()))?;
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
            collect,
            include_failed: raw.include_failed,
            on_conflict: on_conflict.unwrap_or_default(),
            allow_empty: raw.allow_empty,
            requires: raw.requires,
            tolerates: raw.tolerates,
            fallbacks: raw.fallbacks,
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
        if self.collect.is_some_and(CollectMode::collects_files)
            && self.after.len() > MAX_COLLECT_INPUTS
        {
            return Err(invalid(
                "collect",
                format!(
                    "can stage at most {MAX_COLLECT_INPUTS} inputs (got {})",
                    self.after.len()
                ),
            ));
        }
        if self.requires.len() > MAX_REQUIRES_LABELS {
            return Err(invalid(
                "requires",
                format!(
                    "must name at most {MAX_REQUIRES_LABELS} labels (got {})",
                    self.requires.len()
                ),
            ));
        }
        for (i, label) in self.requires.iter().enumerate() {
            if !valid_label(label) {
                return Err(invalid("requires", format!("invalid label {label:?}")));
            }
            if self.requires[..i].contains(label) {
                return Err(invalid("requires", format!("duplicate label {label}")));
            }
        }
        if self.tolerates.len() > MAX_TOLERATES {
            return Err(invalid(
                "tolerates",
                format!(
                    "must name at most {MAX_TOLERATES} taints (got {})",
                    self.tolerates.len()
                ),
            ));
        }
        for (i, taint) in self.tolerates.iter().enumerate() {
            if !valid_label(taint) {
                return Err(invalid("tolerates", format!("invalid label {taint:?}")));
            }
            if self.tolerates[..i].contains(taint) {
                return Err(invalid("tolerates", format!("duplicate label {taint}")));
            }
        }
        if self.fallbacks.len() > MAX_FALLBACKS {
            return Err(invalid(
                "fallbacks",
                format!(
                    "must name at most {MAX_FALLBACKS} choices (got {})",
                    self.fallbacks.len()
                ),
            ));
        }
        for (i, choice) in self.fallbacks.iter().enumerate() {
            check_name("fallbacks", &choice.provider)?;
            if let Some(model) = &choice.model {
                check_len("fallbacks", model, MAX_MODEL_BYTES)?;
            }
            if self.provider.as_deref() == Some(choice.provider.as_str()) {
                return Err(invalid(
                    "fallbacks",
                    format!("duplicate provider {} (primary)", choice.provider),
                ));
            }
            if self.fallbacks[..i]
                .iter()
                .any(|previous| previous.provider == choice.provider)
            {
                return Err(invalid(
                    "fallbacks",
                    format!("duplicate provider {}", choice.provider),
                ));
            }
        }
        if self.is_any_provider() {
            if self.model.is_some() {
                return Err(invalid("model", "must not be set when provider is \"any\""));
            }
            if !self.fallbacks.is_empty() {
                return Err(invalid(
                    "fallbacks",
                    "must be empty when provider is \"any\"",
                ));
            }
        }
        Ok(())
    }

    /// Whether this job leaves the choice of provider and model to dispatch.
    pub fn is_any_provider(&self) -> bool {
        self.provider.as_deref() == Some(ANY_PROVIDER)
    }

    /// A stable JSON encoding: fields in declaration order, absent options omitted. Equal specs
    /// always produce the same bytes.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).expect("a JobSpec always serialises")
    }
}

/// A change to a queued or blocked job. Absent fields stay as they are; a present vector replaces
/// the stored list, so an empty vector clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JobEdit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Vec<Choice>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tolerates: Option<Vec<String>>,
    /// Replacement dependencies. An empty list clears them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<JobId>>,
    /// Replaces the collect mode. `"none"` clears an explicit mode.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_collect_option"
    )]
    pub collect: Option<CollectMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_failed: Option<bool>,
    /// Replaces the conflict policy. `"fail"` clears an explicit policy.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_conflict_option"
    )]
    pub on_conflict: Option<OnConflict>,
}

fn deserialize_collect_option<'de, D>(deserializer: D) -> Result<Option<CollectMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .as_deref()
        .map(|text| CollectMode::parse(text).map_err(serde::de::Error::custom))
        .transpose()
}

impl JobEdit {
    /// Applies this edit to a copy of `spec` and validates the result. Switching to
    /// `provider = "any"` clears a previous model and fallbacks; setting either explicitly with
    /// `any` is rejected.
    pub fn apply(&self, spec: &JobSpec) -> Result<JobSpec, JobSpecError> {
        let mut next = spec.clone();
        if let Some(provider) = &self.provider {
            next.provider = Some(provider.clone());
        }
        if let Some(model) = &self.model {
            next.model = Some(model.clone());
        }
        if let Some(fallbacks) = &self.fallbacks {
            next.fallbacks = fallbacks.clone();
        }
        if let Some(requires) = &self.requires {
            next.requires = requires.clone();
        }
        if let Some(tolerates) = &self.tolerates {
            next.tolerates = tolerates.clone();
        }
        if let Some(after) = &self.after {
            next.after = after.clone();
        }
        if let Some(collect) = self.collect {
            next.collect = Some(collect);
        }
        if let Some(include_failed) = self.include_failed {
            next.include_failed = include_failed;
        }
        if let Some(on_conflict) = self.on_conflict {
            next.on_conflict = on_conflict;
        }
        if next.is_any_provider() {
            if self.model.is_some() || self.fallbacks.as_ref().is_some_and(|f| !f.is_empty()) {
                return Err(invalid("model", "must not be set with provider \"any\""));
            }
            next.model = None;
            next.fallbacks.clear();
        }
        next.validate()?;
        Ok(next)
    }
}

fn deserialize_conflict_option<'de, D>(deserializer: D) -> Result<Option<OnConflict>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .as_deref()
        .map(|text| OnConflict::parse(text).map_err(serde::de::Error::custom))
        .transpose()
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

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_conflict(value: &OnConflict) -> bool {
    *value == OnConflict::Fail
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
            collect: None,
            include_failed: false,
            on_conflict: Default::default(),
            allow_empty: false,
            requires: Vec::new(),
            tolerates: Vec::new(),
            fallbacks: Vec::new(),
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

        spec = sample();
        spec.requires = (0..=MAX_REQUIRES_LABELS)
            .map(|i| format!("label-{i}"))
            .collect();
        assert_eq!(error_field(&spec), "requires");

        spec = sample();
        spec.requires = vec!["bad label".to_owned()];
        assert_eq!(error_field(&spec), "requires");

        spec = sample();
        spec.requires = vec!["gpu".to_owned(), "gpu".to_owned()];
        assert_eq!(error_field(&spec), "requires");

        spec = sample();
        spec.tolerates = (0..=MAX_TOLERATES).map(|i| format!("taint-{i}")).collect();
        assert_eq!(error_field(&spec), "tolerates");

        spec = sample();
        spec.tolerates = vec!["bad label".to_owned()];
        assert_eq!(error_field(&spec), "tolerates");

        spec = sample();
        spec.tolerates = vec!["homelab".to_owned(), "homelab".to_owned()];
        assert_eq!(error_field(&spec), "tolerates");

        spec = sample();
        spec.fallbacks = (0..=MAX_FALLBACKS)
            .map(|i| Choice {
                provider: format!("peer-{i}"),
                model: None,
            })
            .collect();
        assert_eq!(error_field(&spec), "fallbacks");

        spec = sample();
        spec.fallbacks = vec![Choice {
            provider: "bad/provider".to_owned(),
            model: None,
        }];
        assert_eq!(error_field(&spec), "fallbacks");

        spec = sample();
        spec.fallbacks = vec![Choice {
            provider: "peer-a".to_owned(),
            model: Some(String::new()),
        }];
        assert_eq!(error_field(&spec), "fallbacks");

        spec = sample();
        spec.provider = Some("peer-a".to_owned());
        spec.fallbacks = vec![Choice {
            provider: "peer-a".to_owned(),
            model: None,
        }];
        assert_eq!(error_field(&spec), "fallbacks");

        spec = sample();
        spec.fallbacks = vec![
            Choice {
                provider: "peer-a".to_owned(),
                model: Some("one".to_owned()),
            },
            Choice {
                provider: "peer-a".to_owned(),
                model: Some("two".to_owned()),
            },
        ];
        assert_eq!(error_field(&spec), "fallbacks");
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
        assert!(spec.requires.is_empty());
        assert!(spec.tolerates.is_empty());
        assert!(spec.fallbacks.is_empty());
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
    fn collect_modes_and_aliases_parse_to_the_expected_modes() {
        for (text, mode) in [
            ("none", CollectMode::None),
            ("files", CollectMode::Files),
            ("apply", CollectMode::Apply),
            ("both", CollectMode::Both),
            ("OFF", CollectMode::None),
            ("False", CollectMode::None),
            ("diffs", CollectMode::Files),
            ("DIFF", CollectMode::Files),
            ("patches", CollectMode::Files),
            ("patch", CollectMode::Files),
        ] {
            assert_eq!(CollectMode::parse(text).unwrap(), mode, "{text}");
            assert_eq!(text.parse::<CollectMode>().unwrap(), mode, "{text}");
        }
        assert_eq!(CollectMode::parse("nope").unwrap_err().value(), "nope");
        assert!(CollectMode::parse(" nope ").is_err());
    }

    #[test]
    fn explicit_collect_overrides_stack_dependencies() {
        assert_eq!(effective_collect(None, true), CollectMode::Apply);
        assert_eq!(effective_collect(None, false), CollectMode::None);
        assert_eq!(
            effective_collect(Some(CollectMode::None), true),
            CollectMode::None
        );
        assert_eq!(
            effective_collect(Some(CollectMode::Files), true),
            CollectMode::Files
        );
        assert!(CollectMode::Files.collects_files());
        assert!(!CollectMode::Files.applies_diffs());
        assert!(CollectMode::Apply.applies_diffs());
        assert!(!CollectMode::Apply.collects_files());
        assert!(CollectMode::Both.collects_files());
        assert!(CollectMode::Both.applies_diffs());
    }

    #[test]
    fn collect_defaults_to_absent_and_round_trips_every_mode() {
        let json = r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.collect, None);
        assert!(!spec.include_failed);
        assert!(!spec.canonical_json().contains("collect"));
        assert!(!spec.canonical_json().contains("include_failed"));

        for mode in [
            CollectMode::None,
            CollectMode::Files,
            CollectMode::Apply,
            CollectMode::Both,
        ] {
            let mut spec = sample();
            spec.collect = Some(mode);
            spec.include_failed = true;
            let json = spec.canonical_json();
            assert!(
                json.contains(&format!("\"collect\":\"{}\"", mode.as_str())),
                "{json}"
            );
            assert!(json.contains("\"include_failed\":true"), "{json}");
            let parsed: JobSpec = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, spec);
            assert_eq!(parsed.canonical_json(), json);
        }
    }

    #[test]
    fn collect_aliases_are_accepted_on_the_wire_and_reported_by_field() {
        for alias in ["off", "false", "diffs", "diff", "patches", "patch"] {
            let json = format!(
                r#"{{"repo":"https://example.net/r.git","git_ref":"main","task":"go","collect":"{alias}"}}"#
            );
            let spec: JobSpec = serde_json::from_str(&json).unwrap();
            assert!(spec.validate().is_ok(), "{alias}");
        }
        let json =
            r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go","collect":"nope"}"#;
        let error = serde_json::from_str::<JobSpec>(json).unwrap_err();
        assert!(error.to_string().contains("collect"), "{error}");
    }

    #[test]
    fn include_failed_round_trips_through_job_edit() {
        let spec = sample();
        let edit: JobEdit =
            serde_json::from_str(r#"{"collect":"diffs","include_failed":true}"#).unwrap();
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.collect, Some(CollectMode::Files));
        assert!(next.include_failed);

        let clear: JobEdit =
            serde_json::from_str(r#"{"collect":"none","include_failed":false}"#).unwrap();
        let next = clear.apply(&next).unwrap();
        assert_eq!(next.collect, Some(CollectMode::None));
        assert!(!next.include_failed);

        let mut spec = sample();
        spec.collect = Some(CollectMode::Files);
        let edit = JobEdit::default();
        assert_eq!(edit.apply(&spec).unwrap().collect, Some(CollectMode::Files));
    }

    #[test]
    fn conflict_modes_parse_without_aliases_and_round_trip() {
        for (text, mode) in [
            ("fail", OnConflict::Fail),
            ("MARKERS", OnConflict::Markers),
            ("Skip", OnConflict::Skip),
        ] {
            assert_eq!(OnConflict::parse(text).unwrap(), mode, "{text}");
            assert_eq!(text.parse::<OnConflict>().unwrap(), mode, "{text}");
            assert_eq!(mode.as_str().parse::<OnConflict>().unwrap(), mode);
            assert_eq!(mode.to_string(), mode.as_str());
        }
        let error = OnConflict::parse("nope").unwrap_err();
        assert_eq!(error.value(), "nope");
        assert!(error.to_string().contains("fail, markers, skip"));
        assert!(OnConflict::parse(" three-way ").is_err());
    }

    #[test]
    fn on_conflict_defaults_to_fail_and_is_omitted_from_canonical_json() {
        let json = r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.on_conflict, OnConflict::Fail);
        assert!(!spec.canonical_json().contains("on_conflict"));

        for mode in [OnConflict::Fail, OnConflict::Markers, OnConflict::Skip] {
            let mut spec = sample();
            spec.on_conflict = mode;
            let json = spec.canonical_json();
            assert_eq!(
                json.contains("\"on_conflict\""),
                mode != OnConflict::Fail,
                "{json}"
            );
            let parsed: JobSpec = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, spec);
            assert_eq!(parsed.canonical_json(), json);
        }
    }

    #[test]
    fn invalid_on_conflict_is_reported_by_field_on_the_wire() {
        let json = r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go","on_conflict":"merge"}"#;
        let error = serde_json::from_str::<JobSpec>(json).unwrap_err();
        assert!(error.to_string().contains("on_conflict"), "{error}");

        let edit: Result<JobEdit, _> = serde_json::from_str(r#"{"on_conflict":"merge"}"#);
        assert!(edit.is_err());
    }

    #[test]
    fn on_conflict_round_trips_through_job_edit() {
        let spec = sample();
        let edit: JobEdit = serde_json::from_str(r#"{"on_conflict":"markers"}"#).unwrap();
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.on_conflict, OnConflict::Markers);

        let clear: JobEdit = serde_json::from_str(r#"{"on_conflict":"fail"}"#).unwrap();
        let next = clear.apply(&next).unwrap();
        assert_eq!(next.on_conflict, OnConflict::Fail);

        let mut spec = sample();
        spec.on_conflict = OnConflict::Skip;
        let edit = JobEdit::default();
        assert_eq!(edit.apply(&spec).unwrap().on_conflict, OnConflict::Skip);
    }

    #[test]
    fn allow_empty_defaults_false_and_is_omitted_when_false() {
        let json = r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert!(!spec.allow_empty);
        assert!(!spec.canonical_json().contains("allow_empty"));

        let mut spec = sample();
        spec.allow_empty = true;
        let json = spec.canonical_json();
        assert!(json.contains("\"allow_empty\":true"), "{json}");
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn label_requirements_round_trip_and_are_omitted_when_empty() {
        let mut spec = sample();
        assert!(!spec.canonical_json().contains("requires"));
        spec.requires = vec!["php8.3".to_owned(), "region=eu".to_owned()];
        let json = spec.canonical_json();
        assert!(json.contains("\"requires\""));
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);
    }

    #[test]
    fn tolerations_round_trip_and_are_omitted_when_empty() {
        let mut spec = sample();
        assert!(!spec.canonical_json().contains("tolerates"));
        spec.tolerates = vec!["homelab".to_owned(), "region=eu".to_owned()];
        assert!(spec.validate().is_ok());
        let json = spec.canonical_json();
        assert!(json.contains("\"tolerates\""));
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);
    }

    #[test]
    fn fallbacks_round_trip_and_are_omitted_when_empty() {
        let mut spec = sample();
        assert!(!spec.canonical_json().contains("fallbacks"));
        spec.provider = Some("peer-a".to_owned());
        spec.model = Some("primary-model".to_owned());
        spec.fallbacks = vec![
            Choice {
                provider: "peer-b".to_owned(),
                model: Some("other-model".to_owned()),
            },
            Choice {
                provider: "peer-c".to_owned(),
                model: None,
            },
        ];
        assert!(spec.validate().is_ok());
        let json = spec.canonical_json();
        assert!(json.contains("\"fallbacks\""));
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);
        assert!(!json.contains("peer-c:"));
    }

    #[test]
    fn a_fallback_without_a_model_keeps_the_primary_model_choice_open() {
        let json = r#"{"repo":"https://example.net/r.git","git_ref":"main","task":"go",
            "provider":"peer-a","model":"big","fallbacks":[{"provider":"peer-b"}]}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.fallbacks[0].provider, "peer-b");
        assert_eq!(spec.fallbacks[0].model, None);
        assert_eq!(spec.model.as_deref(), Some("big"));
    }

    #[test]
    fn hostile_task_text_still_round_trips_and_is_left_for_display_to_sanitise() {
        let mut spec = sample();
        spec.task = "nul\u{0} esc\u{1b}[31m bidi\u{202e}".to_owned();
        assert!(spec.validate().is_ok());
        let parsed: JobSpec = serde_json::from_str(&spec.canonical_json()).unwrap();
        assert_eq!(parsed.task, spec.task);
    }

    #[test]
    fn any_provider_is_valid_without_a_model_or_fallbacks() {
        let mut spec = sample();
        spec.provider = Some(ANY_PROVIDER.to_owned());
        assert!(spec.is_any_provider());
        assert!(spec.validate().is_ok());
        let parsed: JobSpec = serde_json::from_str(&spec.canonical_json()).unwrap();
        assert_eq!(parsed, spec);
    }

    #[test]
    fn any_provider_refuses_a_model_and_fallbacks() {
        let mut spec = sample();
        spec.provider = Some(ANY_PROVIDER.to_owned());
        spec.model = Some("big".to_owned());
        assert_eq!(error_field(&spec), "model");
        spec.model = None;
        spec.fallbacks = vec![Choice {
            provider: "peer-b".to_owned(),
            model: None,
        }];
        assert_eq!(error_field(&spec), "fallbacks");
    }

    #[test]
    fn job_edit_replaces_fields_and_clears_lists() {
        let mut spec = sample();
        spec.provider = Some("old".to_owned());
        spec.model = Some("old-model".to_owned());
        spec.fallbacks = vec![Choice {
            provider: "peer".to_owned(),
            model: None,
        }];
        spec.requires = vec!["gpu".to_owned()];
        spec.tolerates = vec!["homelab".to_owned()];
        let edit = JobEdit {
            provider: Some("new".to_owned()),
            model: None,
            fallbacks: Some(Vec::new()),
            requires: Some(Vec::new()),
            tolerates: Some(Vec::new()),
            after: None,
            collect: None,
            include_failed: None,
            on_conflict: Default::default(),
        };
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.provider.as_deref(), Some("new"));
        assert_eq!(next.model, spec.model);
        assert!(next.fallbacks.is_empty());
        assert!(next.requires.is_empty());
        assert!(next.tolerates.is_empty());
        assert_eq!(spec.fallbacks.len(), 1);
    }

    #[test]
    fn job_edit_to_any_clears_a_model_and_fallbacks_but_rejects_explicit_ones() {
        let mut spec = sample();
        spec.provider = Some("old".to_owned());
        spec.model = Some("old-model".to_owned());
        spec.fallbacks = vec![Choice {
            provider: "peer".to_owned(),
            model: None,
        }];
        let edit = JobEdit {
            provider: Some(ANY_PROVIDER.to_owned()),
            ..JobEdit::default()
        };
        let next = edit.apply(&spec).unwrap();
        assert!(next.is_any_provider());
        assert_eq!(next.model, None);
        assert!(next.fallbacks.is_empty());

        let bad = JobEdit {
            model: Some("gpt".to_owned()),
            fallbacks: Some(Vec::new()),
            ..edit
        };
        assert_eq!(bad.apply(&spec).unwrap_err().field, "model");
    }

    #[test]
    fn job_edit_rejects_invalid_names_and_too_many_dependencies() {
        let spec = sample();
        let edit = JobEdit {
            provider: Some("bad name".to_owned()),
            ..JobEdit::default()
        };
        assert_eq!(edit.apply(&spec).unwrap_err().field, "provider");

        let edit = JobEdit {
            after: Some(vec![JobId::new(); 17]),
            ..JobEdit::default()
        };
        assert_eq!(edit.apply(&spec).unwrap_err().field, "after");
    }
}
