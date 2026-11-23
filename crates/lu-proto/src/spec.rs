//! The job submission. Every field is range-checked and `validate` names the field at fault.
//! [`JobSpec::canonical_json`] is stable for equal specs, so the store can compare payloads byte
//! for byte to tell a repeated idempotency key from a conflicting one.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::{is_name_byte, JobId};
use crate::label::valid_label;
use crate::limits::{
    DEFAULT_PRIORITY, DEFAULT_TIMEOUT_SECS, MAX_AFTER_JOBS, MAX_CACHE_DIRS, MAX_CACHE_DIR_BYTES,
    MAX_COLLECT_INPUTS, MAX_CONTEXT_BYTES, MAX_EXPECT_GLOB_BYTES, MAX_EXPECT_ITEMS,
    MAX_EXPECT_PATH_BYTES, MAX_FALLBACKS, MAX_GIT_REF_BYTES, MAX_GROUP_BYTES,
    MAX_IDEMPOTENCY_KEY_BYTES, MAX_IDLE_TIMEOUT_SECS, MAX_MODEL_BYTES, MAX_NAME_BYTES,
    MAX_ORIGIN_BYTES, MAX_ORIGIN_REF_BYTES, MAX_PRIORITY, MAX_PUBLISH_BRANCH_BYTES,
    MAX_PUBLISH_GATES, MAX_REPO_BYTES, MAX_REQUIRES_LABELS, MAX_SPAWN_REPOS, MAX_STACK_PATHS,
    MAX_STACK_PATH_BYTES, MAX_STUCK_AFTER_SECS, MAX_TASK_BYTES, MAX_TIMEOUT_SECS, MAX_TOLERATES,
    MIN_PRIORITY, MIN_STUCK_AFTER_SECS, MIN_TIMEOUT_SECS,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectMode {
    None,
    Files,
    #[default]
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

/// How long a node keeps a job's workspace after the job ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspacePolicy {
    /// Delete as soon as the controller has the final artifacts and no dependant needs the tree.
    #[default]
    Ephemeral,
    /// Keep on failure for the agent's `failed_ttl`; delete at once on success.
    KeepOnFailure,
    /// Keep for the agent's `keep_ttl`.
    Keep,
}

/// The canonical spellings accepted by [`WorkspacePolicy::parse`], in help-text order.
pub const WORKSPACE_POLICIES: [&str; 3] = ["ephemeral", "keep_on_failure", "keep"];

impl WorkspacePolicy {
    /// Parses a canonical spelling, case-insensitively.
    pub fn parse(value: &str) -> Result<Self, WorkspacePolicyError> {
        let text = value.trim();
        for name in WORKSPACE_POLICIES {
            if text.eq_ignore_ascii_case(name) {
                return Ok(match name {
                    "ephemeral" => Self::Ephemeral,
                    "keep_on_failure" => Self::KeepOnFailure,
                    _ => Self::Keep,
                });
            }
        }
        Err(WorkspacePolicyError {
            value: value.to_owned(),
        })
    }

    /// The canonical wire spelling, also used in `JobSpec::canonical_json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::KeepOnFailure => "keep_on_failure",
            Self::Keep => "keep",
        }
    }
}

impl FromStr for WorkspacePolicy {
    type Err = WorkspacePolicyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for WorkspacePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `workspace` value that is not a canonical policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePolicyError {
    value: String,
}

impl WorkspacePolicyError {
    /// The rejected input.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for WorkspacePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid workspace policy {:?}; expected one of {}",
            self.value,
            WORKSPACE_POLICIES.join(", ")
        )
    }
}

impl std::error::Error for WorkspacePolicyError {}

/// Default branch template. `<key>` is replaced with the job's idempotency key.
pub const DEFAULT_PUBLISH_BRANCH: &str = "snapshot/<key>";

/// Where a publish attempt pushes its commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishTarget {
    /// Push a new `snapshot/<key>` branch. This is the only default.
    #[default]
    Snapshot,
    /// Fast-forward the job's own source branch inside the same repository. Every layer of
    /// configuration plus an operator approval is required before this is allowed.
    SourceBranch,
}

impl PublishTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::SourceBranch => "source_branch",
        }
    }
}

impl fmt::Display for PublishTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What happens when a publish gate reports a failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateFailureMode {
    /// Hold the branch: do not push.
    #[default]
    Hold,
    /// Push to `<branch>-failing-gates` so reviewers can inspect the failure.
    PushLabelled,
}

/// Canonical spellings for [`GateFailureMode`].
pub const GATE_FAILURE_MODES: [&str; 2] = ["hold", "push_labelled"];

impl GateFailureMode {
    pub fn parse(value: &str) -> Result<Self, GateFailureModeError> {
        match value.trim() {
            "hold" => Ok(Self::Hold),
            "push_labelled" => Ok(Self::PushLabelled),
            _ => Err(GateFailureModeError {
                value: value.to_owned(),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::PushLabelled => "push_labelled",
        }
    }
}

impl FromStr for GateFailureMode {
    type Err = GateFailureModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for GateFailureMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateFailureModeError {
    value: String,
}

impl GateFailureModeError {
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for GateFailureModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid gate failure mode {:?}; expected one of {}",
            self.value,
            GATE_FAILURE_MODES.join(", ")
        )
    }
}

impl std::error::Error for GateFailureModeError {}

/// Job-spec side of publishing. The controller config owns the remote URL and push credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishSpec {
    /// Branch template; `<key>` is substituted with the job idempotency key.
    #[serde(default)]
    pub branch: Option<String>,
    /// Names of configured gates to run on the proposed tree, in order.
    #[serde(default)]
    pub gates: Vec<String>,
    #[serde(default)]
    pub on_gate_failure: GateFailureMode,
    /// Name of a `[publish.remotes.<name>]` entry. Defaults to the only configured remote.
    #[serde(default)]
    pub remote: Option<String>,
    /// Push a snapshot branch (the default) or the job's own source branch.
    #[serde(default)]
    pub target: PublishTarget,
    /// Origin-side allowance for source-branch pushes. An operator sets this in the job spec or
    /// in the intake rule; a webhook payload can never set it.
    #[serde(default)]
    pub allow_source_branch: bool,
}

/// Strict raw form used to convert text-valued enums with field-named errors.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPublishSpec {
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) gates: Vec<String>,
    #[serde(default)]
    pub(crate) on_gate_failure: Option<String>,
    #[serde(default)]
    pub(crate) remote: Option<String>,
    #[serde(default)]
    pub(crate) target: Option<String>,
    #[serde(default)]
    pub(crate) allow_source_branch: bool,
}

impl TryFrom<RawPublishSpec> for PublishSpec {
    type Error = JobSpecError;

    fn try_from(raw: RawPublishSpec) -> Result<Self, Self::Error> {
        let on_gate_failure = match raw.on_gate_failure.as_deref() {
            None => GateFailureMode::Hold,
            Some(text) => GateFailureMode::parse(text).map_err(|e| JobSpecError {
                field: "publish.on_gate_failure",
                reason: e.to_string(),
            })?,
        };
        let target = match raw.target.as_deref() {
            None => PublishTarget::Snapshot,
            Some("snapshot") => PublishTarget::Snapshot,
            Some("source_branch") => PublishTarget::SourceBranch,
            Some(other) => {
                return Err(JobSpecError {
                    field: "publish.target",
                    reason: format!(
                        "invalid publish target {other:?}; expected snapshot or source_branch"
                    ),
                })
            }
        };
        Ok(Self {
            branch: raw.branch,
            gates: raw.gates,
            on_gate_failure,
            remote: raw.remote,
            target,
            allow_source_branch: raw.allow_source_branch,
        })
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

/// How unmet expectations affect a run the harness finished successfully.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExpectMode {
    /// Finish `done` with an `expect_failed` outcome note.
    #[default]
    Suspect,
    /// Finish `failed` with failure class `execution`.
    Fail,
}

/// The canonical spellings accepted by [`ExpectMode::parse`].
pub const EXPECT_MODES: [&str; 2] = ["suspect", "fail"];

impl ExpectMode {
    /// Parses a canonical spelling, case-insensitively.
    pub fn parse(value: &str) -> Result<Self, ExpectModeError> {
        let text = value.trim();
        for name in EXPECT_MODES {
            if text.eq_ignore_ascii_case(name) {
                return Ok(if name == "fail" {
                    Self::Fail
                } else {
                    Self::Suspect
                });
            }
        }
        Err(ExpectModeError {
            value: value.to_owned(),
        })
    }

    /// The canonical wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suspect => "suspect",
            Self::Fail => "fail",
        }
    }
}

impl FromStr for ExpectMode {
    type Err = ExpectModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for ExpectMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An `expect.mode` value that is neither `suspect` nor `fail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectModeError {
    value: String,
}

impl ExpectModeError {
    /// The rejected input.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for ExpectModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid expect mode {:?}; expected one of {}",
            self.value,
            EXPECT_MODES.join(", ")
        )
    }
}

impl std::error::Error for ExpectModeError {}

/// What a successful run must leave behind. Evaluated after the harness exits, against the same
/// exported diff the job uploads; `.lu/` paths are excluded from that diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExpectSpec {
    /// Relative paths that must exist in the working tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Glob patterns that at least one changed path must match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    /// Glob patterns that no changed path may match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbid_changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_diff_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_diff_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "is_suspect")]
    pub mode: ExpectMode,
}

#[derive(Deserialize)]
struct RawExpectSpec {
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    changed: Vec<String>,
    #[serde(default)]
    forbid_changed: Vec<String>,
    #[serde(default)]
    min_diff_bytes: Option<u64>,
    #[serde(default)]
    max_diff_bytes: Option<u64>,
    #[serde(default)]
    mode: Option<String>,
}

impl TryFrom<RawExpectSpec> for ExpectSpec {
    type Error = JobSpecError;

    fn try_from(raw: RawExpectSpec) -> Result<Self, Self::Error> {
        let mode = raw
            .mode
            .as_deref()
            .map(ExpectMode::parse)
            .transpose()
            .map_err(|error| invalid("expect", error.to_string()))?
            .unwrap_or_default();
        let spec = ExpectSpec {
            files: raw.files,
            changed: raw.changed,
            forbid_changed: raw.forbid_changed,
            min_diff_bytes: raw.min_diff_bytes,
            max_diff_bytes: raw.max_diff_bytes,
            mode,
        };
        spec.validate()?;
        Ok(spec)
    }
}

impl<'de> Deserialize<'de> for ExpectSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawExpectSpec::deserialize(deserializer)?;
        ExpectSpec::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl ExpectSpec {
    /// Checks list sizes, path safety, glob syntax and diff bounds.
    pub fn validate(&self) -> Result<(), JobSpecError> {
        for (field, values, max_bytes, glob) in [
            ("files", &self.files, MAX_EXPECT_PATH_BYTES, false),
            ("changed", &self.changed, MAX_EXPECT_GLOB_BYTES, true),
            (
                "forbid_changed",
                &self.forbid_changed,
                MAX_EXPECT_GLOB_BYTES,
                true,
            ),
        ] {
            if values.len() > MAX_EXPECT_ITEMS {
                return Err(invalid(
                    "expect",
                    format!(
                        "{field} must name at most {MAX_EXPECT_ITEMS} entries (got {})",
                        values.len()
                    ),
                ));
            }
            for value in values {
                check_expect_value(value, max_bytes, glob)
                    .map_err(|reason| invalid("expect", format!("{field}: {reason}")))?;
            }
        }
        if let (Some(min), Some(max)) = (self.min_diff_bytes, self.max_diff_bytes) {
            if min > max {
                return Err(invalid(
                    "expect",
                    format!("min_diff_bytes {min} exceeds max_diff_bytes {max}"),
                ));
            }
        }
        Ok(())
    }

    /// Matches a path against a `*` / `**` / `?` glob. `*` and `?` stay within one path segment,
    /// `**` spans segments, and dots have no special meaning.
    pub fn glob_matches(pattern: &str, path: &str) -> bool {
        fn segment(pattern: &str, text: &str) -> bool {
            let mut possible = vec![false; text.chars().count() + 1];
            possible[0] = true;
            for ch in pattern.chars() {
                let mut next = vec![false; possible.len()];
                match ch {
                    '*' => {
                        next[0] = possible[0];
                        for i in 1..next.len() {
                            next[i] = possible[i] || next[i - 1];
                        }
                    }
                    '?' => {
                        next[1..].copy_from_slice(&possible[..possible.len() - 1]);
                    }
                    _ => {
                        for (i, actual) in text.chars().enumerate() {
                            next[i + 1] = possible[i] && actual == ch;
                        }
                    }
                }
                possible = next;
            }
            possible[possible.len() - 1]
        }

        fn parts(pattern: &[&str], path: &[&str]) -> bool {
            let Some((head, rest)) = pattern.split_first() else {
                return path.is_empty();
            };
            if *head == "**" {
                return (0..=path.len()).any(|i| parts(rest, &path[i..]));
            }
            match path.split_first() {
                Some((first, tail)) => segment(head, first) && parts(rest, tail),
                None => false,
            }
        }

        parts(
            &pattern.split('/').collect::<Vec<_>>(),
            &path.split('/').collect::<Vec<_>>(),
        )
    }
}

fn check_expect_value(value: &str, max_bytes: usize, glob: bool) -> Result<(), String> {
    if value.is_empty() {
        return Err("must not be empty".to_owned());
    }
    if value.len() > max_bytes {
        return Err(format!(
            "must be at most {max_bytes} bytes (got {})",
            value.len()
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("must not contain control characters".to_owned());
    }
    if value.contains('\\') {
        return Err("must use forward slashes".to_owned());
    }
    let components = value.split('/').collect::<Vec<_>>();
    if components.iter().any(|part| part.is_empty()) {
        return Err("must have no empty path components".to_owned());
    }
    if components.contains(&".") {
        return Err("must not contain `.` components".to_owned());
    }
    if components.contains(&"..") {
        return Err("must not contain `..` components".to_owned());
    }
    if glob {
        if components.iter().any(|part| part.contains("***")) {
            return Err("`***` is not a valid pattern".to_owned());
        }
        if value.contains(['[', ']']) {
            return Err("character classes are not supported".to_owned());
        }
    }
    Ok(())
}

fn is_suspect(mode: &ExpectMode) -> bool {
    *mode == ExpectMode::Suspect
}

/// Checks a stack-policy glob list. The same conservative grammar as `expect` globs keeps the
/// agent's path matching and the controller's validation in agreement.
pub fn validate_stack_paths(field: &'static str, values: &[String]) -> Result<(), JobSpecError> {
    if values.len() > MAX_STACK_PATHS {
        return Err(invalid(
            field,
            format!(
                "must name at most {MAX_STACK_PATHS} paths (got {})",
                values.len()
            ),
        ));
    }
    for value in values {
        check_expect_value(value, MAX_STACK_PATH_BYTES, true)
            .map_err(|reason| invalid(field, reason))?;
    }
    for (i, value) in values.iter().enumerate() {
        if values[..i].contains(value) {
            return Err(invalid(field, format!("duplicate path {value}")));
        }
    }
    Ok(())
}

/// What the agent does when a job stops producing output and using CPU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StuckPolicy {
    /// Leave the job running. This is the default.
    #[default]
    None,
    /// Stop the harness and fail the job with class `execution`.
    Cancel,
    /// Stop the harness and fail the job with class `node`, so it is retried.
    Retry,
}

/// The canonical spellings of [`StuckPolicy`], in help-text order.
pub const STUCK_POLICIES: [&str; 3] = ["none", "cancel", "retry"];

impl StuckPolicy {
    /// Parses a canonical spelling, case-insensitively.
    pub fn parse(value: &str) -> Result<Self, StuckPolicyError> {
        let text = value.trim();
        for name in STUCK_POLICIES {
            if text.eq_ignore_ascii_case(name) {
                return Ok(match name {
                    "none" => Self::None,
                    "cancel" => Self::Cancel,
                    _ => Self::Retry,
                });
            }
        }
        Err(StuckPolicyError {
            value: value.to_owned(),
        })
    }

    /// The canonical wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cancel => "cancel",
            Self::Retry => "retry",
        }
    }
}

impl FromStr for StuckPolicy {
    type Err = StuckPolicyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for StuckPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `stuck policy` that is not one of the canonical spellings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckPolicyError {
    value: String,
}

impl StuckPolicyError {
    /// The rejected input.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for StuckPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid stuck policy {:?}; expected one of {}",
            self.value,
            STUCK_POLICIES.join(", ")
        )
    }
}

impl std::error::Error for StuckPolicyError {}

/// Resolves the policy and threshold for a job. An absent job value uses the agent default.
pub fn effective_stuck(
    job_policy: Option<StuckPolicy>,
    job_after_secs: Option<u64>,
    default_policy: StuckPolicy,
    default_after_secs: u64,
) -> Option<(StuckPolicy, u64)> {
    let policy = job_policy.unwrap_or(default_policy);
    (policy != StuckPolicy::None).then(|| (policy, job_after_secs.unwrap_or(default_after_secs)))
}

/// The git source a job runs from. `ref` is a branch, tag, commit, or a pull-request ref such
/// as `refs/pull/42/head`. The controller resolves it to a commit before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    /// An https URL (or an operator-configured local path).
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// An optional 40-hex commit used as the base instead of the ref's tip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

impl JobSpec {
    /// A neutral, valid skeleton for callers that build a spec field by field (for example the
    /// operator-written intake rules). The caller must fill `source` and `task` before use.
    pub fn empty() -> Self {
        crate::view::empty_job_spec()
    }
}

impl SourceSpec {
    /// Checks the source fields, returning the first fault with its field name.
    pub fn validate(&self) -> Result<(), JobSpecError> {
        check_repo(&self.repo)?;
        check_len("source.ref", &self.git_ref, MAX_GIT_REF_BYTES)?;
        if let Some(commit) = &self.base {
            check_commit_id(commit)?;
        }
        Ok(())
    }
}

/// A job to run. `harness` defaults to `opencode` and `timeout_secs` to an hour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobSpec {
    /// The canonical wire representation of the repository and ref.
    pub source: SourceSpec,
    /// Normalized fields used by the controller and nodes. Populated from `source` on decode.
    #[serde(skip)]
    pub repo: String,
    #[serde(skip)]
    pub git_ref: String,
    #[serde(skip)]
    pub base_commit: Option<String>,
    /// The prompt text. Control characters are allowed; they are sanitised for display.
    pub task: String,
    /// External text. Written to `.lu/inputs/context.md`; never merged into `task`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Who submitted this job: an intake source name or an operator-chosen label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// A length-capped URL or id pointing back at the submitter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_ref: Option<String>,
    /// Marks `context` as untrusted external text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub untrusted_context: bool,
    /// A draft is stored and visible but is never dispatched until approved.
    #[serde(default, skip_serializing_if = "is_false")]
    pub draft: bool,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    /// What to do when the agent reports this job stuck. Absent uses the agent default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_stuck: Option<StuckPolicy>,
    /// How long the job must be continuously stuck before the agent acts. Absent uses the agent
    /// default; a value of 60 to 86400 seconds is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_after_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Jobs that must be `done` before this one may run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<JobId>,
    /// Which dependency artifacts to stage under `.lu/inputs`.
    #[serde(default, skip_serializing_if = "is_apply")]
    pub collect: CollectMode,
    /// Include `failed` and `cancelled` dependencies as inputs when they stored a diff.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_failed: bool,
    /// How to handle an input patch that does not apply cleanly.
    #[serde(default, skip_serializing_if = "is_default_conflict")]
    pub on_conflict: OnConflict,
    /// Per-job narrowing of the agent's `[stack] union_paths`. Every entry must also appear in
    /// the agent's configured list; the agent refuses a job that widens the policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack_union_paths: Vec<String>,
    /// Per-job narrowing of the agent's `[stack] base_paths`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack_base_paths: Vec<String>,
    /// Whether an empty `diff.patch` is expected, so no `no_changes` note is added.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_empty: bool,
    /// Outputs a successful run must leave behind. Absent means no expectations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<ExpectSpec>,
    /// Labels a node must advertise before this job may run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Node taints this job tolerates. Every taint on a node must appear here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerates: Vec<String>,
    /// Alternatives to the primary provider and model, tried in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<Choice>,
    /// Higher values dispatch first. Omitted means [`DEFAULT_PRIORITY`]; always serialised so
    /// views can show it.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// Optional concurrency group, capped by `[[controller.groups]]` when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Whether this job's harness may submit children. Operators set this at submission; a job
    /// cannot grant itself the right through the scope API.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_spawn: bool,
    /// Whether children of this job may themselves spawn. Defaults to false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spawn_inherit: bool,
    /// Repositories a child may use instead of this job's repo. An allowlist, not a wildcard:
    /// the empty list means "same repo only".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_repos: Vec<String>,
    /// How long the agent keeps this job's workspace. Absent uses the node's configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspacePolicy>,
    /// Directories created inside the workspace before the harness starts, for tool caches such
    /// as Cargo's `target`. They are removed with the workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_dirs: Vec<String>,
    /// Optional publish step, run only by an explicit `lu publish` after this job finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<PublishSpec>,
    /// Time and gate constraints that decide when this job may start. Absent means unconstrained.
    /// Kept last so the canonical JSON of existing specs never moves its other fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<lu_schedule::JobSchedule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJobSpec {
    source: SourceSpec,
    task: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    origin_ref: Option<String>,
    #[serde(default)]
    untrusted_context: bool,
    #[serde(default)]
    draft: bool,
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
    /// Kept as text so an invalid value is reported as an `on_stuck` field error.
    #[serde(default)]
    on_stuck: Option<String>,
    #[serde(default)]
    stuck_after_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<String>,
    #[serde(default)]
    after: Vec<JobId>,
    /// Kept as text so an invalid value is reported as a `collect` field error, with the same
    /// aliases the CLI and MCP accept.
    #[serde(default)]
    collect: Option<String>,
    #[serde(default)]
    include_failed: bool,
    /// Kept as text so an invalid value is reported as an `on_conflict` field error.
    #[serde(default)]
    on_conflict: Option<String>,
    #[serde(default)]
    stack_union_paths: Vec<String>,
    #[serde(default)]
    stack_base_paths: Vec<String>,
    #[serde(default)]
    allow_empty: bool,
    #[serde(default)]
    expect: Option<ExpectSpec>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    tolerates: Vec<String>,
    #[serde(default)]
    fallbacks: Vec<Choice>,
    #[serde(default = "default_priority")]
    priority: u8,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    allow_spawn: bool,
    #[serde(default)]
    spawn_inherit: bool,
    #[serde(default)]
    spawn_repos: Vec<String>,
    /// Kept as text so an invalid value is reported as a `workspace` field error.
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    cache_dirs: Vec<String>,
    #[serde(default)]
    publish: Option<RawPublishSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schedule: Option<lu_schedule::JobSchedule>,
}

impl TryFrom<RawJobSpec> for JobSpec {
    type Error = JobSpecError;

    fn try_from(raw: RawJobSpec) -> Result<Self, Self::Error> {
        let on_stuck = raw
            .on_stuck
            .as_deref()
            .map(StuckPolicy::parse)
            .transpose()
            .map_err(|error| invalid("on_stuck", error.to_string()))?;
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
        let workspace = raw
            .workspace
            .as_deref()
            .map(WorkspacePolicy::parse)
            .transpose()
            .map_err(|error| invalid("workspace", error.to_string()))?;
        let spec = JobSpec {
            repo: raw.source.repo.clone(),
            git_ref: raw.source.git_ref.clone(),
            base_commit: raw.source.base.clone(),
            source: raw.source,
            task: raw.task,
            context: raw.context,
            origin: raw.origin,
            origin_ref: raw.origin_ref,
            untrusted_context: raw.untrusted_context,
            draft: raw.draft,
            harness: raw.harness,
            provider: raw.provider,
            model: raw.model,
            timeout_secs: raw.timeout_secs,
            idle_timeout_secs: raw.idle_timeout_secs,
            on_stuck,
            stuck_after_secs: raw.stuck_after_secs,
            idempotency_key: raw.idempotency_key,
            after: raw.after,
            collect: collect.unwrap_or_default(),
            include_failed: raw.include_failed,
            on_conflict: on_conflict.unwrap_or_default(),
            stack_union_paths: raw.stack_union_paths,
            stack_base_paths: raw.stack_base_paths,
            allow_empty: raw.allow_empty,
            expect: raw.expect,
            requires: raw.requires,
            tolerates: raw.tolerates,
            fallbacks: raw.fallbacks,
            priority: raw.priority,
            group: raw.group,
            allow_spawn: raw.allow_spawn,
            spawn_inherit: raw.spawn_inherit,
            spawn_repos: raw.spawn_repos,
            workspace,
            cache_dirs: raw.cache_dirs,
            publish: raw.publish.map(PublishSpec::try_from).transpose()?,
            schedule: raw.schedule,
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
        self.source.validate()?;
        check_repo(&self.repo)?;
        check_len("git_ref", &self.git_ref, MAX_GIT_REF_BYTES)?;
        if let Some(commit) = &self.base_commit {
            check_commit_id(commit)?;
        }
        check_len("task", &self.task, MAX_TASK_BYTES)?;
        if let Some(context) = &self.context {
            check_len("context", context, MAX_CONTEXT_BYTES)?;
        }
        if let Some(origin) = &self.origin {
            check_len("origin", origin, MAX_ORIGIN_BYTES)?;
            if origin.chars().any(char::is_control) {
                return Err(invalid("origin", "control characters are not allowed"));
            }
        }
        if let Some(origin_ref) = &self.origin_ref {
            check_len("origin_ref", origin_ref, MAX_ORIGIN_REF_BYTES)?;
            if origin_ref.chars().any(char::is_control) {
                return Err(invalid("origin_ref", "control characters are not allowed"));
            }
        }
        check_name("harness", &self.harness)?;
        if let Some(provider) = &self.provider {
            check_name("provider", provider)?;
        }
        if let Some(model) = &self.model {
            check_len("model", model, MAX_MODEL_BYTES)?;
        }
        if !(MIN_PRIORITY..=MAX_PRIORITY).contains(&self.priority) {
            return Err(invalid(
                "priority",
                format!(
                    "must be {MIN_PRIORITY} to {MAX_PRIORITY} (got {})",
                    self.priority
                ),
            ));
        }
        if let Some(group) = &self.group {
            if !valid_group(group) {
                return Err(invalid(
                    "group",
                    format!(
                        "must be 1 to {MAX_GROUP_BYTES} bytes from A-Z a-z 0-9 . _ - (got {group:?})"
                    ),
                ));
            }
        }
        check_timeout("timeout_secs", self.timeout_secs, MAX_TIMEOUT_SECS)?;
        if let Some(idle) = self.idle_timeout_secs {
            check_timeout("idle_timeout_secs", idle, MAX_IDLE_TIMEOUT_SECS)?;
        }
        if let Some(after) = self.stuck_after_secs {
            check_stuck_after(after)?;
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
        if self.collect.collects_files() && self.after.len() > MAX_COLLECT_INPUTS {
            return Err(invalid(
                "collect",
                format!(
                    "can stage at most {MAX_COLLECT_INPUTS} inputs (got {})",
                    self.after.len()
                ),
            ));
        }
        validate_stack_paths("stack_union_paths", &self.stack_union_paths)?;
        validate_stack_paths("stack_base_paths", &self.stack_base_paths)?;
        for path in &self.stack_union_paths {
            if self.stack_base_paths.contains(path) {
                return Err(invalid(
                    "stack_base_paths",
                    format!("{path} is also in stack_union_paths"),
                ));
            }
        }
        if let Some(expect) = &self.expect {
            expect.validate()?;
        }
        if let Some(schedule) = &self.schedule {
            schedule
                .validate("schedule")
                .map_err(|error| invalid("schedule", error.reason()))?;
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
        if self.spawn_repos.len() > MAX_SPAWN_REPOS {
            return Err(invalid(
                "spawn_repos",
                format!(
                    "must name at most {MAX_SPAWN_REPOS} repositories (got {})",
                    self.spawn_repos.len()
                ),
            ));
        }
        for (i, repo) in self.spawn_repos.iter().enumerate() {
            check_repo(repo).map_err(|e| invalid("spawn_repos", e.reason().to_owned()))?;
            if self.spawn_repos[..i].contains(repo) {
                return Err(invalid(
                    "spawn_repos",
                    format!("duplicate repository {repo}"),
                ));
            }
            if repo == &self.repo {
                return Err(invalid(
                    "spawn_repos",
                    "the parent repo is already allowed; do not repeat it",
                ));
            }
        }
        if self.cache_dirs.len() > MAX_CACHE_DIRS {
            return Err(invalid(
                "cache_dirs",
                format!(
                    "must name at most {MAX_CACHE_DIRS} directories (got {})",
                    self.cache_dirs.len()
                ),
            ));
        }
        for (i, dir) in self.cache_dirs.iter().enumerate() {
            check_cache_dir(dir)?;
            if self.cache_dirs[..i].contains(dir) {
                return Err(invalid("cache_dirs", format!("duplicate directory {dir}")));
            }
        }
        if let Some(publish) = &self.publish {
            if let Some(branch) = &publish.branch {
                if branch.is_empty()
                    || branch.len() > MAX_PUBLISH_BRANCH_BYTES
                    || branch.contains("..")
                    || branch.contains('@')
                    || branch.starts_with('/')
                    || branch.ends_with('/')
                    || branch.chars().any(char::is_control)
                {
                    return Err(invalid(
                        "publish.branch",
                        format!("invalid branch template {branch:?}"),
                    ));
                }
            }
            if publish.gates.len() > MAX_PUBLISH_GATES {
                return Err(invalid(
                    "publish.gates",
                    format!("must name at most {MAX_PUBLISH_GATES} gates"),
                ));
            }
            for (i, gate) in publish.gates.iter().enumerate() {
                if gate.is_empty() || gate.len() > MAX_NAME_BYTES {
                    return Err(invalid(
                        "publish.gates",
                        format!("invalid gate name {gate:?}"),
                    ));
                }
                if publish.gates[..i].contains(gate) {
                    return Err(invalid("publish.gates", format!("duplicate gate {gate}")));
                }
            }
            if let Some(remote) = &publish.remote {
                check_name("publish.remote", remote)?;
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

/// Changes to a job's expectations. Present lists replace the stored ones; present size bounds
/// are set, and absent ones stay as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExpectEdit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbid_changed: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_diff_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_diff_bytes: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_expect_mode_option"
    )]
    pub mode: Option<ExpectMode>,
}

fn deserialize_expect_mode_option<'de, D>(deserializer: D) -> Result<Option<ExpectMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .as_deref()
        .map(|text| ExpectMode::parse(text).map_err(serde::de::Error::custom))
        .transpose()
}

/// A change to a queued or blocked job. Absent fields stay as they are; a present vector replaces
/// the stored list, so an empty vector clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JobEdit {
    /// Replacement operator-authored prompt text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Replacement source ref; both the wire source and normalized field are updated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// Replacement publish settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish: Option<PublishSpec>,
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
    /// Replaces the expectations block, or parts of it. An empty list clears that list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<ExpectEdit>,
    /// Replaces the conflict policy. `"fail"` clears an explicit policy.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_conflict_option"
    )]
    pub on_conflict: Option<OnConflict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    /// `None` leaves the group alone, `Some(None)` clears it, `Some(Some(name))` sets it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_group_option"
    )]
    pub group: Option<Option<String>>,
    /// Replaces the stuck policy. `"none"` sets an explicit never-stop policy.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_stuck_policy_option"
    )]
    pub on_stuck: Option<StuckPolicy>,
    /// Replaces the continuous stuck threshold in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_after_secs: Option<u64>,
    /// Grants this job's harness the right to submit children. Only operators may set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_spawn: Option<bool>,
    /// Grants children of this job the right to spawn. Only operators may set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_inherit: Option<bool>,
    /// Replaces the child repository allowlist. An empty list means the parent repo only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_repos: Option<Vec<String>>,
    /// Replaces the scheduling block. `None` leaves it alone, JSON `null` clears it, and an
    /// object sets it. The edited spec is validated, so an invalid block is refused.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_schedule_option"
    )]
    pub schedule: Option<Option<lu_schedule::JobSchedule>>,
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

/// Plain `Option<Option<String>>` cannot tell an absent field from a JSON `null`; here `null`
/// must clear the group, so presence is recorded explicitly.
fn deserialize_group_option<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct GroupVisitor;

    impl<'de> serde::de::Visitor<'de> for GroupVisitor {
        type Value = Option<Option<String>>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a group name or null")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            Option::<String>::deserialize(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(GroupVisitor)
}

/// Same presence rule as `group`: an absent `schedule` means "no change", a JSON `null` means
/// "clear the block", and an object means "set the block".
fn deserialize_schedule_option<'de, D>(
    deserializer: D,
) -> Result<Option<Option<lu_schedule::JobSchedule>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ScheduleVisitor;

    impl<'de> serde::de::Visitor<'de> for ScheduleVisitor {
        type Value = Option<Option<lu_schedule::JobSchedule>>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a schedule block or null")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            lu_schedule::JobSchedule::deserialize(deserializer).map(|schedule| Some(Some(schedule)))
        }
    }

    deserializer.deserialize_option(ScheduleVisitor)
}

fn deserialize_stuck_policy_option<'de, D>(deserializer: D) -> Result<Option<StuckPolicy>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .as_deref()
        .map(|text| StuckPolicy::parse(text).map_err(serde::de::Error::custom))
        .transpose()
}

impl JobEdit {
    /// Applies this edit to a copy of `spec` and validates the result. Switching to
    /// `provider = "any"` clears a previous model and fallbacks; setting either explicitly with
    /// `any` is rejected.
    pub fn apply(&self, spec: &JobSpec) -> Result<JobSpec, JobSpecError> {
        let mut next = spec.clone();
        if let Some(task) = &self.task {
            next.task = task.clone();
        }
        if let Some(source_ref) = &self.source_ref {
            next.git_ref = source_ref.clone();
            next.source.git_ref = source_ref.clone();
        }
        if let Some(publish) = &self.publish {
            next.publish = Some(publish.clone());
        }
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
            next.collect = collect;
        }
        if let Some(include_failed) = self.include_failed {
            next.include_failed = include_failed;
        }
        if let Some(on_conflict) = self.on_conflict {
            next.on_conflict = on_conflict;
        }
        if let Some(expect) = &self.expect {
            let mut merged = next.expect.take().unwrap_or_default();
            if let Some(files) = &expect.files {
                merged.files = files.clone();
            }
            if let Some(changed) = &expect.changed {
                merged.changed = changed.clone();
            }
            if let Some(forbid_changed) = &expect.forbid_changed {
                merged.forbid_changed = forbid_changed.clone();
            }
            if let Some(min) = expect.min_diff_bytes {
                merged.min_diff_bytes = Some(min);
            }
            if let Some(max) = expect.max_diff_bytes {
                merged.max_diff_bytes = Some(max);
            }
            if let Some(mode) = expect.mode {
                merged.mode = mode;
            }
            next.expect = Some(merged);
        }
        if let Some(priority) = self.priority {
            next.priority = priority;
        }
        if let Some(group) = &self.group {
            next.group = group.clone();
        }
        if let Some(policy) = self.on_stuck {
            next.on_stuck = Some(policy);
        }
        if let Some(after) = self.stuck_after_secs {
            next.stuck_after_secs = Some(after);
        }
        if let Some(allow_spawn) = self.allow_spawn {
            next.allow_spawn = allow_spawn;
        }
        if let Some(spawn_inherit) = self.spawn_inherit {
            next.spawn_inherit = spawn_inherit;
        }
        if let Some(spawn_repos) = &self.spawn_repos {
            next.spawn_repos = spawn_repos.clone();
        }
        if let Some(schedule) = &self.schedule {
            next.schedule = schedule.clone();
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

/// Whether `value` is a usable group name: nonempty, at most [`MAX_GROUP_BYTES`], and only
/// `A-Z a-z 0-9 . _ -`. Config and the wire use the same rule.
pub fn valid_group(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_GROUP_BYTES && value.bytes().all(is_name_byte)
}

fn default_harness() -> String {
    "opencode".to_owned()
}

fn default_priority() -> u8 {
    DEFAULT_PRIORITY
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

fn is_apply(value: &CollectMode) -> bool {
    *value == CollectMode::Apply
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_conflict(value: &OnConflict) -> bool {
    *value == OnConflict::Fail
}

/// A cache directory is a relative path with safe, nonempty segments. Absolute paths, `..`,
/// `.` and symlink names are refused so a job cannot name a directory outside its workspace.
fn check_cache_dir(value: &str) -> Result<(), JobSpecError> {
    if value.is_empty() || value.len() > MAX_CACHE_DIR_BYTES {
        return Err(invalid(
            "cache_dirs",
            format!("must be 1 to {MAX_CACHE_DIR_BYTES} bytes"),
        ));
    }
    if value.starts_with('/') || value.contains('\\') {
        return Err(invalid(
            "cache_dirs",
            format!("{value:?} must be a relative path inside the workspace"),
        ));
    }
    for part in value.split('/') {
        if part.is_empty() || part == "." || part == ".." || !part.bytes().all(is_name_byte) {
            return Err(invalid(
                "cache_dirs",
                format!("{value:?} has an unsafe path segment"),
            ));
        }
    }
    Ok(())
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
    if repo_url_parts(repo).is_some_and(|(_, authority, _)| authority.contains('@')) {
        return Err(invalid("repo", "must not contain credentials"));
    }
    Ok(())
}

/// Splits a URL-form repo into scheme, authority and the rest. Local paths and malformed strings
/// are not URLs, so they are left alone.
fn repo_url_parts(repo: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = repo.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some((scheme, &rest[..end], &rest[end..]))
}

/// Removes credentials, query and fragment from a repository URL before it is shown to a client.
/// Existing rows may predate the rule that rejects credentials, so views redact on read as well.
/// Local paths are returned unchanged.
pub fn redact_repo(repo: &str) -> Cow<'_, str> {
    let Some((scheme, authority, rest)) = repo_url_parts(repo) else {
        return Cow::Borrowed(repo);
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    let path = &rest[..path_end];
    if host == authority && path == rest {
        return Cow::Borrowed(repo);
    }
    Cow::Owned(format!("{scheme}://{host}{path}"))
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

fn check_stuck_after(secs: u64) -> Result<(), JobSpecError> {
    if !(MIN_STUCK_AFTER_SECS..=MAX_STUCK_AFTER_SECS).contains(&secs) {
        return Err(invalid(
            "stuck_after_secs",
            format!(
                "must be {MIN_STUCK_AFTER_SECS} to {MAX_STUCK_AFTER_SECS} seconds (got {secs})"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> JobSpec {
        JobSpec {
            source: crate::SourceSpec {
                repo: "https://example.net/repo.git".to_owned(),
                git_ref: "main".to_owned(),
                base: None,
            },
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
            collect: CollectMode::Apply,
            include_failed: false,
            on_conflict: Default::default(),
            stack_union_paths: Vec::new(),
            stack_base_paths: Vec::new(),
            allow_empty: false,
            expect: None,
            requires: Vec::new(),
            on_stuck: None,
            stuck_after_secs: None,
            tolerates: Vec::new(),
            fallbacks: Vec::new(),
            priority: DEFAULT_PRIORITY,
            group: None,
            allow_spawn: false,
            spawn_inherit: false,
            spawn_repos: Vec::new(),
            workspace: None,
            cache_dirs: Vec::new(),
            context: None,
            origin: None,
            origin_ref: None,
            untrusted_context: false,
            draft: false,
            publish: None,
            schedule: None,
        }
    }

    fn error_field(spec: &JobSpec) -> &'static str {
        spec.validate().unwrap_err().field()
    }

    #[test]
    fn stack_policy_overrides_are_validated_and_canonical() {
        let mut spec = sample();
        spec.stack_union_paths.push("CHANGELOG.md".to_owned());
        spec.stack_base_paths.push("Cargo.lock".to_owned());
        assert!(spec.validate().is_ok());
        assert!(spec
            .canonical_json()
            .contains("\"stack_union_paths\":[\"CHANGELOG.md\"]"));

        spec.stack_union_paths.push("../escape".to_owned());
        assert_eq!(error_field(&spec), "stack_union_paths");
        spec.stack_union_paths.pop();
        spec.stack_union_paths.push("Cargo.lock".to_owned());
        assert_eq!(error_field(&spec), "stack_base_paths");
    }

    #[test]
    fn a_well_formed_spec_validates() {
        assert!(sample().validate().is_ok());
        let mut spec = sample();
        spec.repo = "/srv/git/repo".to_owned();
        spec.source.repo = "/srv/git/repo".to_owned();
        spec.provider = Some("peer-a".to_owned());
        spec.model = Some("big-pickle".to_owned());
        spec.idle_timeout_secs = Some(30);
        spec.idempotency_key = Some("ticket:1234".to_owned());
        assert!(spec.validate().is_ok());
    }

    fn credentialed_repo() -> String {
        // Assembled here so no credential-shaped text is stored in the source tree.
        let user = ['v', 'i', 'e', 'w'].iter().collect::<String>();
        let pass = ['o', 'n', 'l', 'y'].iter().collect::<String>();
        format!("https://{user}:{pass}@example.net/repo.git")
    }

    #[test]
    fn userinfo_is_rejected_in_repo_urls() {
        let mut spec = sample();
        let repo = credentialed_repo();
        spec.repo = repo.clone();
        spec.source.repo = repo;
        let error = spec.validate().unwrap_err();
        assert_eq!(error.field(), "repo");
        assert_eq!(error.reason(), "must not contain credentials");

        let mut spec = sample();
        let user_only = format!(
            "https://{}@example.net/repo.git",
            ['v', 'i', 's', 'i', 't', 'o', 'r']
                .iter()
                .collect::<String>()
        );
        spec.repo = user_only.clone();
        spec.source.repo = user_only;
        assert_eq!(error_field(&spec), "repo");
    }

    #[test]
    fn stored_repo_urls_are_redacted_on_read() {
        let repo = credentialed_repo();
        let with_extras = format!("{repo}?branch=main#section");
        assert_eq!(redact_repo(&with_extras), "https://example.net/repo.git");
        assert_eq!(
            redact_repo("https://example.net/repo.git?ref=main"),
            "https://example.net/repo.git"
        );
        assert_eq!(redact_repo("/srv/git/repo"), "/srv/git/repo");
    }

    #[test]
    fn every_field_reports_its_own_error() {
        let mut spec = sample();
        spec.repo = String::new();
        spec.source.repo = String::new();
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.repo = "relative/path".to_owned();
        spec.source.repo = "relative/path".to_owned();
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.repo = format!("https://example.net/{}", "a".repeat(MAX_REPO_BYTES));
        spec.source.repo = format!("https://example.net/{}", "a".repeat(MAX_REPO_BYTES));
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.repo = "https://example.net/a\u{0}b".to_owned();
        spec.source.repo = "https://example.net/a\u{0}b".to_owned();
        assert_eq!(error_field(&spec), "repo");

        spec = sample();
        spec.git_ref = String::new();
        spec.source.git_ref = String::new();
        assert_eq!(error_field(&spec), "source.ref");

        spec = sample();
        spec.git_ref = "a".repeat(MAX_GIT_REF_BYTES + 1);
        spec.source.git_ref = "a".repeat(MAX_GIT_REF_BYTES + 1);
        assert_eq!(error_field(&spec), "source.ref");

        spec = sample();
        spec.base_commit = Some("abc".to_owned());
        spec.source.base = Some("abc".to_owned());
        assert_eq!(error_field(&spec), "base_commit");
        spec = sample();
        spec.base_commit = Some(format!("{}z", "a".repeat(39)));
        spec.source.base = Some(format!("{}z", "a".repeat(39)));
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
        spec.source.git_ref = String::new();
        let error = spec.validate().unwrap_err();
        assert_eq!(error.field(), "source.ref");
        assert!(error.to_string().starts_with("source.ref:"), "{error}");
    }

    #[test]
    fn defaults_fill_in_for_harness_and_timeout() {
        let json =
            r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.harness, "opencode");
        assert_eq!(spec.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(spec.provider, None);
        assert_eq!(spec.base_commit, None);
        assert!(spec.after.is_empty());
        assert_eq!(spec.collect, CollectMode::Apply);
        assert!(spec.requires.is_empty());
        assert!(spec.tolerates.is_empty());
        assert!(spec.fallbacks.is_empty());
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn an_old_style_spec_without_base_commit_still_deserialises() {
        let json =
            r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.base_commit, None);
        assert!(!spec.canonical_json().contains("\"base\""));
    }

    #[test]
    fn a_pinned_spec_serialises_and_round_trips_base_commit() {
        let mut spec = sample();
        spec.base_commit = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
        spec.source.base = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
        let json = spec.canonical_json();
        assert!(json.contains("\"base\""));
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json = r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go","surprise":1}"#;
        let err = serde_json::from_str::<JobSpec>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown field") || err.contains("surprise"),
            "{err}"
        );
    }

    #[test]
    fn canonical_json_is_stable_across_field_order_and_round_trips() {
        let spec = sample();
        let json = spec.canonical_json();
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);

        let reordered = format!(
            r#"{{"task":"{}","timeout_secs":{},"harness":"{}","source":{{"ref":"{}","repo":"{}"}}}}"#,
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
        let positions: Vec<usize> = ["source", "task", "harness", "provider", "timeout_secs"]
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
        assert!(!spec.canonical_json().contains("\"base\""));
        spec.base_commit = Some("a".repeat(40));
        spec.source.base = Some("a".repeat(40));
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
    fn collect_defaults_to_apply_and_is_omitted_only_when_apply() {
        let mut spec = sample();
        assert_eq!(spec.collect, CollectMode::Apply);
        assert!(!spec.canonical_json().contains("\"collect\""));

        spec.collect = CollectMode::None;
        let json = spec.canonical_json();
        assert!(json.contains("\"collect\":\"none\""), "{json}");
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.collect, CollectMode::None);
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
    fn collect_mode_flags() {
        assert!(CollectMode::Files.collects_files());
        assert!(!CollectMode::Files.applies_diffs());
        assert!(CollectMode::Apply.applies_diffs());
        assert!(!CollectMode::Apply.collects_files());
        assert!(CollectMode::Both.collects_files());
        assert!(CollectMode::Both.applies_diffs());
        assert!(!CollectMode::None.collects_files());
        assert!(!CollectMode::None.applies_diffs());
    }

    #[test]
    fn collect_defaults_to_apply_and_round_trips_every_mode() {
        let json =
            r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.collect, CollectMode::Apply);
        assert!(!spec.include_failed);
        assert!(!spec.canonical_json().contains("\"collect\""));
        assert!(!spec.canonical_json().contains("include_failed"));

        for mode in [
            CollectMode::None,
            CollectMode::Files,
            CollectMode::Apply,
            CollectMode::Both,
        ] {
            let mut spec = sample();
            spec.collect = mode;
            spec.include_failed = true;
            let json = spec.canonical_json();
            if mode == CollectMode::Apply {
                assert!(!json.contains("\"collect\""), "{json}");
            } else {
                assert!(
                    json.contains(&format!("\"collect\":\"{}\"", mode.as_str())),
                    "{json}"
                );
            }
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
                r#"{{"source": {{"repo": "https://example.net/r.git", "ref": "main"}},"task":"go","collect":"{alias}"}}"#
            );
            let spec: JobSpec = serde_json::from_str(&json).unwrap();
            assert!(spec.validate().is_ok(), "{alias}");
        }
        let json = r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go","collect":"nope"}"#;
        let error = serde_json::from_str::<JobSpec>(json).unwrap_err();
        assert!(error.to_string().contains("collect"), "{error}");
    }

    #[test]
    fn include_failed_round_trips_through_job_edit() {
        let spec = sample();
        let edit: JobEdit =
            serde_json::from_str(r#"{"collect":"diffs","include_failed":true}"#).unwrap();
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.collect, CollectMode::Files);
        assert!(next.include_failed);

        let clear: JobEdit =
            serde_json::from_str(r#"{"collect":"none","include_failed":false}"#).unwrap();
        let next = clear.apply(&next).unwrap();
        assert_eq!(next.collect, CollectMode::None);
        assert!(!next.include_failed);

        let mut spec = sample();
        spec.collect = CollectMode::Files;
        let edit = JobEdit::default();
        assert_eq!(edit.apply(&spec).unwrap().collect, CollectMode::Files);
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
        let json =
            r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go"}"#;
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
        let json = r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go","on_conflict":"merge"}"#;
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
        let json =
            r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go"}"#;
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
        let json = r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go",
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
            task: None,
            source_ref: None,
            publish: None,
            provider: Some("new".to_owned()),
            model: None,
            fallbacks: Some(Vec::new()),
            requires: Some(Vec::new()),
            tolerates: Some(Vec::new()),
            after: None,
            collect: None,
            include_failed: None,
            expect: None,
            on_conflict: Default::default(),
            priority: None,
            group: None,
            on_stuck: None,
            stuck_after_secs: None,
            allow_spawn: Some(true),
            spawn_inherit: Some(true),
            spawn_repos: Some(vec!["https://example.test/other.git".to_owned()]),
            schedule: None,
        };
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.provider.as_deref(), Some("new"));
        assert_eq!(next.model, spec.model);
        assert!(next.fallbacks.is_empty());
        assert!(next.requires.is_empty());
        assert!(next.tolerates.is_empty());
        assert!(next.allow_spawn);
        assert!(next.spawn_inherit);
        assert_eq!(
            next.spawn_repos,
            vec!["https://example.test/other.git".to_owned()]
        );
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

    fn expected() -> ExpectSpec {
        ExpectSpec {
            files: vec!["REVIEW.md".to_owned()],
            changed: vec!["src/**".to_owned()],
            forbid_changed: vec!["Cargo.lock".to_owned()],
            min_diff_bytes: Some(1),
            max_diff_bytes: Some(5 * 1024 * 1024),
            mode: ExpectMode::Suspect,
        }
    }

    #[test]
    fn expect_defaults_to_absent_and_round_trips() {
        let json =
            r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.expect, None);
        assert!(!spec.canonical_json().contains("expect"));

        let mut spec = sample();
        spec.expect = Some(expected());
        let json = spec.canonical_json();
        assert!(json.contains("\"expect\":{"), "{json}");
        assert!(json.contains("\"files\":[\"REVIEW.md\"]"), "{json}");
        assert!(json.contains("\"changed\":[\"src/**\"]"), "{json}");
        assert!(
            json.contains("\"forbid_changed\":[\"Cargo.lock\"]"),
            "{json}"
        );
        assert!(json.contains("\"min_diff_bytes\":1"), "{json}");
        assert!(json.contains("\"max_diff_bytes\":5242880"), "{json}");
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);
    }

    #[test]
    fn expect_modes_round_trip_and_default_to_suspect() {
        let json = r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go",
            "expect":{"files":["REVIEW.md"],"mode":"fail"}}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        let expect = spec.expect.unwrap();
        assert_eq!(expect.mode, ExpectMode::Fail);
        assert_eq!(ExpectMode::parse(" SUSPECT ").unwrap(), ExpectMode::Suspect);
        assert_eq!(ExpectMode::Fail.as_str(), "fail");
        assert!(ExpectMode::parse("nope").is_err());

        let mut spec = sample();
        spec.expect = Some(expected());
        let parsed: JobSpec = serde_json::from_str(&spec.canonical_json()).unwrap();
        assert_eq!(parsed.expect.unwrap().mode, ExpectMode::Suspect);
    }

    #[test]
    fn expect_validation_reports_the_expect_field() {
        let mut spec = sample();
        let mut expect = expected();
        expect.files = vec!["../escape".to_owned()];
        spec.expect = Some(expect);
        assert_eq!(error_field(&spec), "expect");

        let mut expect = expected();
        expect.files = vec!["/absolute".to_owned()];
        spec.expect = Some(expect);
        assert_eq!(error_field(&spec), "expect");

        for path in ["a/../b", "./REVIEW.md", "dir/", "", "a\\b", "a//b"] {
            let mut expect = expected();
            expect.files = vec![path.to_owned()];
            spec.expect = Some(expect);
            assert_eq!(error_field(&spec), "expect", "{path}");
        }

        let mut expect = expected();
        expect.changed = vec!["src/**/*.rs".to_owned()];
        spec.expect = Some(expect);
        assert!(spec.validate().is_ok());

        for pattern in ["***", "src/[ab]", "../escape", "src//x", "/abs", ""] {
            let mut expect = expected();
            expect.changed = vec![pattern.to_owned()];
            spec.expect = Some(expect);
            assert_eq!(error_field(&spec), "expect", "{pattern}");
        }

        let mut expect = expected();
        expect.files = vec!["f".to_owned(); MAX_EXPECT_ITEMS + 1];
        spec.expect = Some(expect);
        assert_eq!(error_field(&spec), "expect");

        let mut expect = expected();
        expect.changed = vec!["f".to_owned(); MAX_EXPECT_ITEMS + 1];
        spec.expect = Some(expect);
        assert_eq!(error_field(&spec), "expect");

        let mut expect = expected();
        expect.forbid_changed = vec!["f".to_owned(); MAX_EXPECT_ITEMS + 1];
        spec.expect = Some(expect);
        assert_eq!(error_field(&spec), "expect");

        let mut expect = expected();
        expect.min_diff_bytes = Some(10);
        expect.max_diff_bytes = Some(9);
        spec.expect = Some(expect);
        assert_eq!(error_field(&spec), "expect");
    }

    #[test]
    fn expect_glob_matches_segments_wildcards_and_dotfiles() {
        assert!(ExpectSpec::glob_matches("src/**", "src/main.rs"));
        assert!(ExpectSpec::glob_matches("src/**", "src/a/b/main.rs"));
        assert!(ExpectSpec::glob_matches("**/*.rs", "main.rs"));
        assert!(ExpectSpec::glob_matches("**/*.rs", "src/lib.rs"));
        assert!(ExpectSpec::glob_matches("src/*.rs", "src/main.rs"));
        assert!(ExpectSpec::glob_matches("src/file?.rs", "src/file1.rs"));
        assert!(ExpectSpec::glob_matches("*.md", ".hidden.md"));
        assert!(!ExpectSpec::glob_matches("src/*.rs", "src/a/lib.rs"));
        assert!(!ExpectSpec::glob_matches("src/*.rs", "tests/lib.rs"));
        assert!(!ExpectSpec::glob_matches("src/file?.rs", "src/file12.rs"));
        assert!(!ExpectSpec::glob_matches("src/**", "tests/main.rs"));
    }

    #[test]
    fn job_edit_merges_expect_fields_and_creates_a_block() {
        let mut spec = sample();
        spec.expect = Some(expected());
        let edit: JobEdit =
            serde_json::from_str(r#"{"expect":{"changed":[],"mode":"fail","min_diff_bytes":42}}"#)
                .unwrap();
        let next = edit.apply(&spec).unwrap();
        let expect = next.expect.unwrap();
        assert!(expect.changed.is_empty());
        assert_eq!(expect.files, vec!["REVIEW.md".to_owned()]);
        assert_eq!(expect.mode, ExpectMode::Fail);
        assert_eq!(expect.min_diff_bytes, Some(42));
        assert_eq!(expect.max_diff_bytes, Some(5 * 1024 * 1024));

        let edit: JobEdit = serde_json::from_str(r#"{"expect":{"files":["OUT.md"]}}"#).unwrap();
        let next = edit.apply(&sample()).unwrap();
        assert_eq!(next.expect.unwrap().files, vec!["OUT.md".to_owned()]);

        assert!(serde_json::from_str::<JobEdit>(r#"{"expect":{"mode":"nope"}}"#).is_err());
    }

    #[test]
    fn priority_defaults_to_five_and_bounds_are_enforced() {
        let json =
            r#"{"source": {"repo": "https://example.net/r.git", "ref": "main"},"task":"go"}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.priority, DEFAULT_PRIORITY);
        assert!(spec
            .canonical_json()
            .contains(&format!("\"priority\":{DEFAULT_PRIORITY}")));

        for priority in [MIN_PRIORITY, 1, 9, MAX_PRIORITY] {
            let json = format!(
                r#"{{"source": {{"repo": "https://example.net/r.git", "ref": "main"}},"task":"go","priority":{priority}}}"#
            );
            let spec: JobSpec = serde_json::from_str(&json).unwrap();
            assert_eq!(spec.priority, priority);
            assert!(spec
                .canonical_json()
                .contains(&format!("\"priority\":{priority}")));
        }
        for priority in [255, -1] {
            let json = format!(
                r#"{{"source": {{"repo": "https://example.net/r.git", "ref": "main"}},"task":"go","priority":{priority}}}"#
            );
            assert!(serde_json::from_str::<JobSpec>(&json).is_err());
        }
        let mut spec = sample();
        spec.priority = MAX_PRIORITY + 1;
        assert_eq!(error_field(&spec), "priority");
        assert!(!valid_group("bad group"));
        assert!(valid_group("a.b_c-1"));
    }

    #[test]
    fn group_round_trips_and_is_omitted_when_absent() {
        let mut spec = sample();
        assert!(!spec.canonical_json().contains("group"));
        spec.group = Some("nightly".to_owned());
        assert!(spec.validate().is_ok());
        let json = spec.canonical_json();
        assert!(json.contains("\"group\":\"nightly\""));
        let parsed: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), json);

        spec.group = Some("bad group".to_owned());
        assert_eq!(error_field(&spec), "group");
        spec.group = Some("x".repeat(MAX_GROUP_BYTES + 1));
        assert_eq!(error_field(&spec), "group");
    }

    #[test]
    fn job_edit_sets_priority_and_clears_a_group_with_null() {
        let mut spec = sample();
        spec.group = Some("old".to_owned());
        let edit: JobEdit = serde_json::from_str(r#"{"priority":9,"group":"new"}"#).unwrap();
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.priority, 9);
        assert_eq!(next.group.as_deref(), Some("new"));

        let clear: JobEdit = serde_json::from_str(r#"{"group":null}"#).unwrap();
        let next = clear.apply(&next).unwrap();
        assert_eq!(next.group, None);
        assert_eq!(next.priority, 9);

        let bad: JobEdit = serde_json::from_str(r#"{"priority":10}"#).unwrap();
        assert_eq!(bad.apply(&spec).unwrap_err().field, "priority");
        let bad: JobEdit = serde_json::from_str(r#"{"group":"bad group"}"#).unwrap();
        assert_eq!(bad.apply(&spec).unwrap_err().field, "group");
    }

    #[test]
    fn stuck_policy_spellings_are_canonical() {
        for (text, policy) in [
            ("none", StuckPolicy::None),
            ("cancel", StuckPolicy::Cancel),
            ("retry", StuckPolicy::Retry),
        ] {
            assert_eq!(StuckPolicy::parse(text).unwrap(), policy);
            assert_eq!(
                StuckPolicy::parse(&text.to_ascii_uppercase()).unwrap(),
                policy
            );
            assert_eq!(policy.as_str(), text);
            assert_eq!(policy.to_string(), text);
        }
        assert_eq!(StuckPolicy::parse("later").unwrap_err().value(), "later");
    }

    #[test]
    fn stuck_after_secs_is_range_checked() {
        let mut spec = sample();
        spec.stuck_after_secs = Some(MIN_STUCK_AFTER_SECS - 1);
        assert_eq!(error_field(&spec), "stuck_after_secs");
        spec.stuck_after_secs = Some(MAX_STUCK_AFTER_SECS + 1);
        assert_eq!(error_field(&spec), "stuck_after_secs");
        spec.stuck_after_secs = Some(MIN_STUCK_AFTER_SECS);
        assert!(spec.validate().is_ok());
        spec.stuck_after_secs = Some(MAX_STUCK_AFTER_SECS);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn stuck_fields_round_trip_and_report_their_field() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json.get("on_stuck").is_none());
        assert!(json.get("stuck_after_secs").is_none());

        let mut spec = sample();
        spec.on_stuck = Some(StuckPolicy::Cancel);
        spec.stuck_after_secs = Some(120);
        let decoded: JobSpec = serde_json::from_str(&spec.canonical_json()).unwrap();
        assert_eq!(decoded, spec);

        let mut value = serde_json::to_value(&spec).unwrap();
        value["on_stuck"] = serde_json::json!("later");
        let error = serde_json::from_value::<JobSpec>(value)
            .unwrap_err()
            .to_string();
        assert!(error.contains("on_stuck"), "{error}");

        let mut value = serde_json::to_value(&spec).unwrap();
        value["stuck_after_secs"] = serde_json::json!(59);
        let error = serde_json::from_value::<JobSpec>(value)
            .unwrap_err()
            .to_string();
        assert!(error.contains("stuck_after_secs"), "{error}");
    }

    #[test]
    fn job_edit_replaces_stuck_fields() {
        let spec = sample();
        let edit = JobEdit {
            on_stuck: Some(StuckPolicy::Retry),
            stuck_after_secs: Some(90),
            ..JobEdit::default()
        };
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.on_stuck, Some(StuckPolicy::Retry));
        assert_eq!(next.stuck_after_secs, Some(90));

        let edit = JobEdit {
            on_stuck: Some(StuckPolicy::None),
            stuck_after_secs: Some(59),
            ..JobEdit::default()
        };
        assert_eq!(edit.apply(&spec).unwrap_err().field, "stuck_after_secs");
    }

    #[test]
    fn schedule_round_trips_and_is_omitted_when_absent() {
        let spec = sample();
        assert_eq!(spec.schedule, None);
        assert!(!spec.canonical_json().contains("schedule"));

        let json = r#"{"source":{"repo":"https://example.net/r.git","ref":"main"},"task":"go",
            "schedule":{"not_before":1700000000,"deadline":1700003600,
            "on_deadline":"cancel","window":"nightly","fit":"5m"}}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        let schedule = spec.schedule.as_ref().unwrap();
        assert_eq!(schedule.not_before, Some(1_700_000_000));
        assert_eq!(schedule.deadline, Some(1_700_003_600));
        assert_eq!(schedule.on_deadline, lu_schedule::OnDeadline::Cancel);
        assert_eq!(schedule.window.as_deref(), Some("nightly"));
        assert_eq!(schedule.fit, lu_schedule::Fit::Seconds(300));
        assert!(spec.validate().is_ok());

        let canonical = spec.canonical_json();
        assert!(canonical.contains("\"schedule\":{"), "{canonical}");
        let parsed: JobSpec = serde_json::from_str(&canonical).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.canonical_json(), canonical);
    }

    #[test]
    fn schedule_rejects_unknown_fields_and_invalid_values() {
        let prefix = r#"{"source":{"repo":"https://example.net/r.git","ref":"main"},"task":"go","schedule":"#;
        let error = serde_json::from_str::<JobSpec>(&format!("{prefix}{{\"later\":1}}}}"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unknown field") || error.contains("later"),
            "{error}"
        );

        let error = serde_json::from_str::<JobSpec>(&format!(
            "{prefix}{{\"deadline\":5,\"not_before\":10}}}}"
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("schedule"), "{error}");
        assert_eq!(
            error_field(&{
                let mut spec = sample();
                spec.schedule = Some(lu_schedule::JobSchedule {
                    not_before: Some(10),
                    deadline: Some(5),
                    ..Default::default()
                });
                spec
            }),
            "schedule"
        );
    }

    #[test]
    fn job_edit_sets_and_clears_the_schedule_block() {
        let spec = sample();
        let edit: JobEdit =
            serde_json::from_str(r#"{"schedule":{"not_before":1700000000}}"#).unwrap();
        let expected = lu_schedule::JobSchedule {
            not_before: Some(1_700_000_000),
            ..Default::default()
        };
        assert_eq!(edit.schedule, Some(Some(expected.clone())));
        let next = edit.apply(&spec).unwrap();
        assert_eq!(next.schedule, Some(expected));

        let clear: JobEdit = serde_json::from_str(r#"{"schedule":null}"#).unwrap();
        assert_eq!(clear.schedule, Some(None));
        assert_eq!(
            serde_json::to_value(&clear).unwrap()["schedule"],
            serde_json::Value::Null
        );
        assert_eq!(clear.apply(&next).unwrap().schedule, None);

        let no_change: JobEdit = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(no_change.schedule, None);
        assert_eq!(no_change.apply(&next).unwrap().schedule, next.schedule);

        let invalid: JobEdit = serde_json::from_str(r#"{"schedule":{"not_before":-1}}"#).unwrap();
        assert_eq!(invalid.apply(&spec).unwrap_err().field, "schedule");
    }

    #[test]
    fn effective_stuck_resolves_job_values_then_agent_defaults() {
        assert_eq!(
            effective_stuck(None, None, StuckPolicy::Cancel, 600),
            Some((StuckPolicy::Cancel, 600))
        );
        assert_eq!(
            effective_stuck(Some(StuckPolicy::Retry), Some(90), StuckPolicy::Cancel, 600),
            Some((StuckPolicy::Retry, 90))
        );
        assert_eq!(
            effective_stuck(Some(StuckPolicy::None), Some(90), StuckPolicy::Cancel, 600),
            None
        );
        assert_eq!(
            effective_stuck(Some(StuckPolicy::Retry), None, StuckPolicy::None, 600),
            Some((StuckPolicy::Retry, 600))
        );
    }
}
