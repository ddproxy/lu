//! JSON views of jobs and nodes, shared by the controller that builds them and the clients that
//! decode them. Field names and optionality match `docs/api.md`; do not add `deny_unknown_fields`,
//! because older controllers and agents may send extra fields.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    FailureClass, HarnessState, JobId, JobSpec, JobState, NodeName, ProviderModel, ProviderStatus,
    PublishRecord, WorkspacePolicy, WorkspaceState,
};

/// A submitted job as returned by the controller.
///
/// `outcome_note` is absent unless the controller has one; `warnings` appears only on submit
/// replies, where an empty list is still present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub id: JobId,
    pub state: JobState,
    #[serde(default = "empty_job_spec")]
    pub spec: JobSpec,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub node: Option<NodeName>,
    #[serde(default)]
    pub assigned_provider: Option<String>,
    #[serde(default)]
    pub assigned_model: Option<String>,
    #[serde(default)]
    pub epoch: u64,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub after: Vec<JobId>,
    #[serde(default)]
    pub waiting_on: Vec<JobId>,
    /// Parent of a job submitted through a harness's scoped socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_job_id: Option<JobId>,
    /// Root of the spawn tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_job_id: Option<JobId>,
    /// Distance from the root; top-level jobs are depth 0.
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub waiting_reason: Option<String>,
    /// Commit the agent actually staged this job from, recorded before dependency patches are
    /// applied. Dependents inherit it when they have no pinned base of their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_commit: Option<String>,
    /// Why a blocked job is waiting; `predecessor failed` after a failed or cancelled dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// Wall-clock deadline at which a blocked dependent fails with class `dependency`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_note: Option<String>,
    /// Size of `diff.patch`, present for terminal jobs (zero when none was stored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_bytes: Option<u64>,
    /// Number of secrets replaced in stored artifacts and logs.
    #[serde(default)]
    pub scrubbed_hits: u64,
    /// Whether `diff.patch` was changed by scrubbing and may no longer apply cleanly.
    #[serde(default)]
    pub patch_scrubbed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
    /// Audit enqueue/write failures on the controller that built this view, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_write_errors: Option<u64>,
    /// Publish status for a job with a publish step, after an attempt has been made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<PublishRecord>,
    /// Effective publish target computed from config and spec, shown even before an attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_target: Option<String>,
}

impl JobView {
    /// A view for a route that answered 204 (cancel) or with only an id (submit).
    pub fn placeholder(id: JobId, state: JobState) -> Self {
        Self {
            id,
            state,
            spec: empty_job_spec(),
            attempt: 0,
            node: None,
            assigned_provider: None,
            assigned_model: None,
            epoch: 0,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            finished_at: None,
            failure_class: None,
            summary: None,
            exit_code: None,
            artifacts: Vec::new(),
            cancel_requested: false,
            after: Vec::new(),
            waiting_on: Vec::new(),
            parent_job_id: None,
            root_job_id: None,
            depth: 0,
            waiting_reason: None,
            staged_commit: None,
            blocked_reason: None,
            blocked_until: None,
            outcome_note: None,
            diff_bytes: None,
            scrubbed_hits: 0,
            patch_scrubbed: false,
            warnings: None,
            audit_write_errors: None,
            publish: None,
            publish_target: None,
        }
    }

    /// Advice from a submit reply; no warnings on other replies.
    pub fn warnings(&self) -> &[String] {
        self.warnings.as_deref().unwrap_or_default()
    }
}

/// Why a provider on a node is cordoned. `until` is a wall-clock Unix time for a failure cordon,
/// and null for a Reiki cordon that lifts after healthy heartbeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CordonView {
    pub provider: String,
    pub source: String,
    pub reason: String,
    pub set_at: i64,
    #[serde(default)]
    pub until: Option<i64>,
    #[serde(default)]
    pub lifts_after_healthy_heartbeats: Option<u32>,
}

/// One node's heartbeat and drain state. Nullable fields stay explicit nulls, as the controller
/// has always sent them; only `reiki` is an opaque forwarded document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    pub name: NodeName,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub last_seen: u64,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub draining: bool,
    #[serde(default)]
    pub harness: Option<HarnessState>,
    #[serde(default)]
    pub providers: Vec<ProviderStatus>,
    /// Healthy provider/model pairs offered for `provider = "any"` jobs.
    #[serde(default)]
    pub provider_models: Vec<ProviderModel>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub label_sources: BTreeMap<String, String>,
    #[serde(default)]
    pub taints: Vec<String>,
    #[serde(default)]
    pub current_job: Option<JobId>,
    #[serde(default)]
    pub cordon_details: Vec<CordonView>,
    #[serde(default)]
    pub reiki: Option<serde_json::Value>,
    #[serde(default)]
    pub load1: Option<f64>,
    #[serde(default)]
    pub mem_available_kb: Option<u64>,
    #[serde(default)]
    pub mem_total_kb: Option<u64>,
    #[serde(default)]
    pub stuck: u64,
    /// Bytes in workspaces currently on disk.
    #[serde(default)]
    pub workspace_bytes: u64,
    /// Workspaces currently on disk.
    #[serde(default)]
    pub workspace_count: u64,
    /// Bytes reclaimed by workspace deletion since the agent started.
    #[serde(default)]
    pub workspace_reclaimed_bytes_total: u64,
    /// Workspaces removed by eviction.
    #[serde(default)]
    pub workspace_evictions_total: u64,
    /// Failed deletion attempts, all retried by the agent.
    #[serde(default)]
    pub workspace_delete_errors_total: u64,
    /// The agent's workspace listing from its latest heartbeat, oldest first.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceView>,
}

/// One workspace from an agent's `lu node <name> --workspaces` listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub job_key: String,
    pub state: WorkspaceState,
    pub bytes: u64,
    pub age_secs: u64,
    pub policy: WorkspacePolicy,
}

/// Result of an operator's workspace clean request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCleanResult {
    pub queued: u64,
    pub refused_running: u64,
    pub missing: u64,
}

/// The controller's answer to an agent's "may I delete this workspace yet?" question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRelease {
    pub can_delete: bool,
    /// A short, operator-safe explanation; never a filesystem path.
    pub reason: String,
}

impl NodeView {
    /// A view for a drain route that answered 204.
    pub fn placeholder(name: NodeName, draining: bool) -> Self {
        Self {
            name,
            version: None,
            last_seen: 0,
            online: false,
            draining,
            harness: None,
            providers: Vec::new(),
            provider_models: Vec::new(),
            labels: Vec::new(),
            label_sources: BTreeMap::new(),
            taints: Vec::new(),
            current_job: None,
            cordon_details: Vec::new(),
            reiki: None,
            load1: None,
            mem_available_kb: None,
            mem_total_kb: None,
            stuck: 0,
            workspace_bytes: 0,
            workspace_count: 0,
            workspace_reclaimed_bytes_total: 0,
            workspace_evictions_total: 0,
            workspace_delete_errors_total: 0,
            workspaces: Vec::new(),
        }
    }
}

pub(crate) fn empty_job_spec() -> JobSpec {
    JobSpec {
        source: crate::SourceSpec {
            repo: String::new(),
            git_ref: String::new(),
            base: None,
        },
        repo: String::new(),
        git_ref: String::new(),
        base_commit: None,
        task: String::new(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: crate::DEFAULT_TIMEOUT_SECS,
        idle_timeout_secs: None,
        idempotency_key: None,
        after: Vec::new(),
        collect: crate::CollectMode::Apply,
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
        priority: crate::DEFAULT_PRIORITY,
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
    }
}
