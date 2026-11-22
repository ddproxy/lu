//! Wire types for job-scoped requests.
//!
//! The same bounded JSON decoder is used for the harness-facing Unix socket and for the agent's
//! forwarding request to the controller. It is deliberately small, strict and free of I/O so it
//! can be fuzzed directly. Nothing in this module authenticates anything: authentication and
//! authorisation are the controller's job.

use serde::{Deserialize, Serialize};

use crate::{
    printable, validate_idempotency_key, JobId, MAX_AFTER_JOBS, MAX_IDEMPOTENCY_KEY_BYTES,
    MAX_MODEL_BYTES, MAX_NAME_BYTES, MAX_REPO_BYTES, MAX_TASK_BYTES, MAX_TIMEOUT_SECS,
};

/// A socket line or controller body larger than this is refused before parsing. It leaves room for
/// JSON framing around the largest task the `[scope] max_task_bytes` setting may allow.
pub const MAX_SCOPE_REQUEST_BYTES: usize = MAX_TASK_BYTES + 16 * 1024;
/// Largest reply accepted from a scope socket. The controller caps artifact reads at 10 MiB, but a
/// lossily decoded artifact can expand under JSON escaping, so leave room for that and for the
/// MCP layer's structured copy.
pub const MAX_SCOPE_REPLY_BYTES: usize = 128 * 1024 * 1024;
/// Summaries shown through scope tools are capped at 4 KiB.
pub const MAX_SCOPE_SUMMARY_BYTES: usize = 4096;
/// The source tag attached to every result produced from child-job data.
pub const SCOPE_SOURCE: &str = "child_job";

/// Every route accepted under `/v1/scope/`.
pub const SCOPE_ROUTES: [&str; 6] = [
    "job_context",
    "submit_child",
    "list_children",
    "get_child",
    "get_child_artifact",
    "cancel_child",
];

/// Names used by `limit_exceeded` errors and the `lu_scope_limit_exceeded_total` metric.
pub const SCOPE_LIMITS: [&str; 7] = [
    "scope_enabled",
    "max_children_per_job",
    "max_descendants_per_root",
    "max_depth",
    "max_task_bytes",
    "max_spawn_per_minute",
    "max_artifact_read_bytes",
];

/// The one action carried by a scope request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeAction {
    JobContext,
    SubmitChild,
    ListChildren,
    GetChild,
    GetChildArtifact,
    CancelChild,
}

impl ScopeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JobContext => "job_context",
            Self::SubmitChild => "submit_child",
            Self::ListChildren => "list_children",
            Self::GetChild => "get_child",
            Self::GetChildArtifact => "get_child_artifact",
            Self::CancelChild => "cancel_child",
        }
    }

    /// Parses only the fixed route names; an unknown route is rejected without a 404 leak.
    pub fn parse(value: &str) -> Option<Self> {
        SCOPE_ROUTES
            .iter()
            .find(|name| **name == value)
            .map(|name| match *name {
                "job_context" => Self::JobContext,
                "submit_child" => Self::SubmitChild,
                "list_children" => Self::ListChildren,
                "get_child" => Self::GetChild,
                "get_child_artifact" => Self::GetChildArtifact,
                _ => Self::CancelChild,
            })
    }
}

impl std::fmt::Display for ScopeAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fields accepted by scope actions. Only the fields named by an action are legal for it; the
/// decoder enforces that so a harness cannot use one tool to carry another tool's payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ScopePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<JobId>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

/// A decoded scope request. `job_id` is always the job whose harness made the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeRequest {
    pub job_id: JobId,
    pub epoch: u64,
    pub action: ScopeAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<ScopePayload>,
}

impl ScopeRequest {
    pub fn new(job_id: JobId, epoch: u64, action: ScopeAction) -> Self {
        Self {
            job_id,
            epoch,
            action,
            payload: None,
        }
    }

    pub fn with_payload(mut self, payload: ScopePayload) -> Self {
        self.payload = Some(payload);
        self
    }

    /// Checks action-specific field presence and static field bounds. The controller applies all
    /// policy (clamps, limits, lineage) after this passes.
    pub fn validate_shape(&self) -> Result<(), ScopeDecodeError> {
        let p = self.payload.as_ref();
        let field = |name: &str| {
            p.is_some_and(|payload| match name {
                "task" => payload.task.is_some(),
                "repo" => payload.repo.is_some(),
                "git_ref" => payload.git_ref.is_some(),
                "provider" => payload.provider.is_some(),
                "model" => payload.model.is_some(),
                "priority" => payload.priority.is_some(),
                "timeout_secs" => payload.timeout_secs.is_some(),
                "requires" => payload.requires.is_some(),
                "after" => payload.after.is_some(),
                "idempotency_key" => payload.idempotency_key.is_some(),
                "child" => payload.child.is_some(),
                "artifact" => payload.artifact.is_some(),
                _ => false,
            })
        };
        let allowed: &[&str] = match self.action {
            ScopeAction::JobContext | ScopeAction::ListChildren => &[],
            ScopeAction::SubmitChild => &[
                "task",
                "repo",
                "provider",
                "model",
                "priority",
                "timeout_secs",
                "requires",
                "after",
                "idempotency_key",
            ],
            ScopeAction::GetChild | ScopeAction::CancelChild => &["child"],
            ScopeAction::GetChildArtifact => &["child", "artifact"],
        };
        let required: &[&str] = match self.action {
            ScopeAction::SubmitChild => &["task"],
            ScopeAction::GetChild | ScopeAction::CancelChild => &["child"],
            ScopeAction::GetChildArtifact => &["child", "artifact"],
            ScopeAction::JobContext | ScopeAction::ListChildren => &[],
        };
        for name in required {
            if !field(name) {
                return Err(ScopeDecodeError::new(format!("missing {name}")));
            }
        }
        if let Some(p) = p {
            for (name, present) in [
                ("task", p.task.is_some()),
                ("repo", p.repo.is_some()),
                ("git_ref", p.git_ref.is_some()),
                ("provider", p.provider.is_some()),
                ("model", p.model.is_some()),
                ("priority", p.priority.is_some()),
                ("timeout_secs", p.timeout_secs.is_some()),
                ("requires", p.requires.is_some()),
                ("after", p.after.is_some()),
                ("idempotency_key", p.idempotency_key.is_some()),
                ("child", p.child.is_some()),
                ("artifact", p.artifact.is_some()),
            ] {
                if present && !allowed.contains(&name) {
                    return Err(ScopeDecodeError::new(format!(
                        "{name} is not allowed for {}",
                        self.action
                    )));
                }
            }
        }
        if self.epoch == 0 {
            return Err(ScopeDecodeError::new("epoch must be positive"));
        }
        if let Some(p) = p {
            if let Some(task) = &p.task {
                if task.is_empty() || task.len() > MAX_TASK_BYTES {
                    return Err(ScopeDecodeError::new("invalid task length"));
                }
            }
            if let Some(repo) = &p.repo {
                if repo.is_empty() || repo.len() > MAX_REPO_BYTES {
                    return Err(ScopeDecodeError::new("invalid repo length"));
                }
            }
            if let Some(git_ref) = &p.git_ref {
                if git_ref.is_empty() || git_ref.len() > crate::MAX_GIT_REF_BYTES {
                    return Err(ScopeDecodeError::new("invalid git_ref length"));
                }
            }
            if let Some(provider) = &p.provider {
                if provider.is_empty() || provider.len() > MAX_NAME_BYTES {
                    return Err(ScopeDecodeError::new("invalid provider length"));
                }
            }
            if let Some(model) = &p.model {
                if model.is_empty() || model.len() > MAX_MODEL_BYTES {
                    return Err(ScopeDecodeError::new("invalid model length"));
                }
            }
            if let Some(timeout) = p.timeout_secs {
                if timeout == 0 || timeout > MAX_TIMEOUT_SECS {
                    return Err(ScopeDecodeError::new("invalid timeout_secs"));
                }
            }
            if let Some(requires) = &p.requires {
                if requires.len() > crate::MAX_REQUIRES_LABELS
                    || requires.iter().any(|label| !crate::valid_label(label))
                {
                    return Err(ScopeDecodeError::new("invalid requires"));
                }
            }
            if let Some(after) = &p.after {
                if after.len() > MAX_AFTER_JOBS {
                    return Err(ScopeDecodeError::new("invalid after length"));
                }
            }
            if let Some(key) = &p.idempotency_key {
                if key.len() > MAX_IDEMPOTENCY_KEY_BYTES || validate_idempotency_key(key).is_err() {
                    return Err(ScopeDecodeError::new("invalid idempotency_key"));
                }
            }
            if let Some(artifact) = &p.artifact {
                if artifact != "diff.patch" && artifact != "report.json" {
                    return Err(ScopeDecodeError::new("invalid artifact"));
                }
            }
        }
        Ok(())
    }
}

/// Why a scope request was refused before it reached policy checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDecodeError {
    reason: String,
}

impl ScopeDecodeError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for ScopeDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ScopeDecodeError {}

/// Decodes one bounded, single-line JSON request. Both the agent socket and controller use this,
/// which keeps their parsers identical and fuzzable.
pub fn decode_scope_request(input: &[u8]) -> Result<ScopeRequest, ScopeDecodeError> {
    if input.is_empty() || input.len() > MAX_SCOPE_REQUEST_BYTES {
        return Err(ScopeDecodeError::new("request size out of range"));
    }
    if input.contains(&b'\n') || input.contains(&b'\r') {
        return Err(ScopeDecodeError::new("request must be one line"));
    }
    let request: ScopeRequest = serde_json::from_slice(input)
        .map_err(|e| ScopeDecodeError::new(format!("invalid JSON: {e}")))?;
    request.validate_shape()?;
    Ok(request)
}

/// Decodes one raw socket line, trimming only its trailing newline.
pub fn decode_socket_line(input: &[u8]) -> Result<ScopeRequest, ScopeDecodeError> {
    let line = input.strip_suffix(b"\n").unwrap_or(input);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    decode_scope_request(line)
}

/// A dependency as exposed to a harness: identifier and status only, never task text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeDependency {
    pub job_id: JobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// A child as exposed to its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeChild {
    pub job_id: JobId,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// A scrubbed artifact read through the scope API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeArtifact {
    pub job_id: JobId,
    pub name: String,
    /// The stored (already scrubbed) bytes, decoded lossily for display.
    pub content: String,
    pub truncated: bool,
    pub scrubbed: bool,
}

/// Typed result payloads. Every one is safe to display as data, never as instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScopeResult {
    JobContext {
        job_id: JobId,
        state: String,
        spec: Box<crate::JobSpec>,
        #[serde(default)]
        dependencies: Vec<ScopeDependency>,
    },
    SubmitChild {
        job_id: JobId,
    },
    ListChildren {
        #[serde(default)]
        children: Vec<ScopeChild>,
    },
    GetChild {
        child: ScopeChild,
    },
    GetChildArtifact {
        artifact: ScopeArtifact,
    },
    CancelChild {
        job_id: JobId,
        state: String,
    },
}

fn scope_source() -> String {
    SCOPE_SOURCE.to_owned()
}

/// Envelope returned by every scope route and every scoped MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeResponse {
    /// Always `child_job`; lets a harness label tool output as untrusted data.
    #[serde(default = "scope_source")]
    pub source: String,
    #[serde(default)]
    pub untrusted: bool,
    pub result: ScopeResult,
}

impl ScopeResponse {
    pub fn new(result: ScopeResult) -> Self {
        Self {
            source: scope_source(),
            untrusted: true,
            result,
        }
    }
}

/// Error envelope returned by a scope route or the agent's socket. It never carries task text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeErrorResponse {
    #[serde(default = "scope_source")]
    pub source: String,
    #[serde(default)]
    pub untrusted: bool,
    pub error: ScopeErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeErrorDetail {
    pub code: String,
    /// A fixed or sanitised message, safe to display but still untrusted data.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
}

impl ScopeErrorResponse {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            source: scope_source(),
            untrusted: true,
            error: ScopeErrorDetail {
                code: code.into(),
                message: message.into(),
                limit: None,
            },
        }
    }

    pub fn with_limit(mut self, limit: impl Into<String>) -> Self {
        self.error.limit = Some(limit.into());
        self
    }
}

/// Caps a summary to [`MAX_SCOPE_SUMMARY_BYTES`] without breaking UTF-8.
pub fn short_summary(summary: Option<&str>) -> Option<String> {
    short_summary_with_limit(summary, MAX_SCOPE_SUMMARY_BYTES)
}

/// Caps a summary to the configured limit without breaking UTF-8.
pub fn short_summary_with_limit(summary: Option<&str>, max: usize) -> Option<String> {
    let text = summary?.trim();
    if text.is_empty() {
        return None;
    }
    let max = max.clamp(1, MAX_SCOPE_SUMMARY_BYTES);
    Some(crate::truncate_chars(&printable(text), max))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(json: &str) -> Result<ScopeRequest, ScopeDecodeError> {
        decode_scope_request(json.as_bytes())
    }

    #[test]
    fn rejects_unknown_fields_and_wrong_action_fields() {
        let id = JobId::new();
        let base = format!("{{\"job_id\":\"{id}\",\"epoch\":1,\"action\":\"job_context\"");
        assert!(request(&format!("{base},\"bogus\":1}}")).is_err());
        assert!(request(&format!("{base},\"payload\":{{\"task\":\"x\"}}}}")).is_err());
    }

    #[test]
    fn requires_expected_fields() {
        let id = JobId::new();
        assert!(request(&format!(
            "{{\"job_id\":\"{id}\",\"epoch\":1,\"action\":\"submit_child\"}}"
        ))
        .is_err());
        assert!(request(&format!(
            "{{\"job_id\":\"{id}\",\"epoch\":1,\"action\":\"submit_child\",\"payload\":{{\"task\":\"x\"}}}}"
        ))
        .is_ok());
    }

    #[test]
    fn bounds_size_and_newlines() {
        assert!(decode_scope_request(b"").is_err());
        let valid = format!(
            "{{\"job_id\":\"{}\",\"epoch\":1,\"action\":\"job_context\"}}",
            JobId::new()
        );
        assert!(decode_socket_line(format!("{valid}\r\n").as_bytes()).is_ok());
        assert!(decode_scope_request(format!("{valid}\n").as_bytes()).is_err());
        let big = vec![b'a'; MAX_SCOPE_REQUEST_BYTES + 1];
        assert!(decode_scope_request(&big).is_err());
    }

    #[test]
    fn short_summary_is_bounded() {
        let long = "x".repeat(MAX_SCOPE_SUMMARY_BYTES + 100);
        assert!(short_summary(Some(&long)).unwrap().len() <= MAX_SCOPE_SUMMARY_BYTES);
    }
}
