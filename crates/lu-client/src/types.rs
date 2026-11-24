//! Request and response types for the controller API.
//!
//! The controller is the source of these strings, so display code must pass them through
//! [`lu_proto::printable`]; the types do not pretend to sanitise untrusted text on construction.

use lu_proto::{
    AgentEvent, FailureClass, HarnessState, Heartbeat, JobId, JobState, NodeName, Outcome,
    ProviderModel, ProviderStatus, WorkspaceView,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use lu_proto::{
    AgentSelfView, HostStats, JobStats, JobView, NodeStats, NodeView, ProcessStats,
    WorkspaceCleanResult, WorkspaceRelease,
};

/// Largest JSON response the client will read. Artifacts have their own per-file cap.
pub const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;

/// One of a job's `after` dependencies, as the agent sees it before staging its own tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyView {
    pub job_id: JobId,
    pub state: JobState,
    /// The size of the dependency's `diff.patch`; zero means it changed nothing.
    #[serde(default)]
    pub diff_bytes: u64,
    /// The dependency's idempotency key, or its short id when it had none.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub node: Option<NodeName>,
    #[serde(default)]
    pub assigned_provider: Option<String>,
    #[serde(default)]
    pub assigned_model: Option<String>,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    #[serde(default)]
    pub summary: Option<String>,
    /// Commit the dependency was staged from before its patches were applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_commit: Option<String>,
}

/// The result of an operator retry. `requeued` lists the dependents restored after the root;
/// `waiting_dependents` reports how many dependents are still blocked waiting for a retry or TTL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryReply {
    pub job: JobView,
    #[serde(default)]
    pub requeued: Vec<JobId>,
    #[serde(default)]
    pub waiting_dependents: u64,
}

/// A page of jobs. The controller may add fields later; unknown ones are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListJobs {
    #[serde(default)]
    pub jobs: Vec<JobView>,
    #[serde(default)]
    pub next: Option<String>,
}

/// A page of draft jobs, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDrafts {
    #[serde(default)]
    pub drafts: Vec<JobView>,
    #[serde(default)]
    pub next: Option<String>,
}

/// The node list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListNodes {
    #[serde(default)]
    pub nodes: Vec<NodeView>,
}

/// A configured repository mirror and its latest sync.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoView {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub synced_at: Option<u64>,
    #[serde(default)]
    pub heads: BTreeMap<String, String>,
}

/// The configured repository list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRepos {
    #[serde(default)]
    pub repos: Vec<RepoView>,
}

/// A controller status snapshot. The controller may add fields later; unknown ones are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusView {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub schema_version: i64,
    #[serde(default)]
    pub db_bytes: u64,
    #[serde(default)]
    pub recovered_on_boot: u64,
    /// Blocked dependents waiting for a retry or TTL expiry.
    #[serde(default)]
    pub waiting_dependents: u64,
    #[serde(default)]
    pub jobs: BTreeMap<String, u64>,
    #[serde(default)]
    pub oldest_queued_secs: u64,
    #[serde(default)]
    pub drafts: u64,
    #[serde(default)]
    pub oldest_draft_secs: u64,
    #[serde(default)]
    pub nodes: u64,
    #[serde(default)]
    pub nodes_online: u64,
    #[serde(default)]
    pub lease_secs: u64,
    /// Audit enqueue/write failures since the controller started (zero when auditing is off).
    #[serde(default)]
    pub audit_write_errors: u64,
    #[serde(default)]
    pub scope_enabled: bool,
    #[serde(default)]
    pub scope_max_depth: u32,
    #[serde(default)]
    pub scope_max_children_per_job: u64,
    #[serde(default)]
    pub scope_max_descendants_per_root: u64,
}

/// What an agent sends when it asks for work. The token already identifies the node, so `node`
/// is optional; it is included when an agent wants the controller to verify it explicitly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PollRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    /// Provider/model pairs the agent can serve for `provider = "any"` jobs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub provider_models: Vec<ProviderModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reiki: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    /// False when the agent's harness is unhealthy. Absent means healthy for older agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_ok: Option<bool>,
    /// Free space on the work filesystem as a percentage, from 0 to 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_pct: Option<u8>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub label_sources: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub taints: Vec<String>,
    /// Bytes in workspaces currently on disk.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub workspace_bytes: u64,
    /// Workspaces currently on disk.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub workspace_count: u64,
    /// Bytes reclaimed by workspace deletion since the agent started.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub workspace_reclaimed_bytes_total: u64,
    /// Workspaces removed by high-water or size-cap eviction.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub workspace_evictions_total: u64,
    /// Failed deletion attempts; failures are retried and never silently ignored.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub workspace_delete_errors_total: u64,
    /// Scratch directories removed by housekeeping since the agent started.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gc_removed_total: u64,
    /// Bytes freed by housekeeping since the agent started.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gc_bytes_freed_total: u64,
    /// Workspaces the agent is tracking, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_secs: Option<u64>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl From<Heartbeat> for PollRequest {
    fn from(beat: Heartbeat) -> Self {
        Self {
            node: Some(beat.node),
            harness: Some(beat.harness),
            version: beat.version,
            providers: beat.providers,
            provider_models: beat.provider_models,
            reiki: beat.reiki,
            host: beat.host,
            harness_ok: beat.harness_ok,
            disk_free_pct: beat.disk_free_pct,
            jobs: beat.jobs,
            labels: beat.labels,
            label_sources: beat.label_sources,
            taints: beat.taints,
            workspace_bytes: beat.workspace_bytes,
            workspace_count: beat.workspace_count,
            workspace_reclaimed_bytes_total: beat.workspace_reclaimed_bytes_total,
            workspace_evictions_total: beat.workspace_evictions_total,
            workspace_delete_errors_total: beat.workspace_delete_errors_total,
            gc_removed_total: beat.gc_removed_total,
            gc_bytes_freed_total: beat.gc_bytes_freed_total,
            workspaces: beat.workspaces,
            wait_secs: None,
        }
    }
}

impl PollRequest {
    /// A request from a node that only needs an assignment.
    pub fn for_node(node: NodeName) -> Self {
        Self {
            node: Some(node),
            ..Self::default()
        }
    }
}

/// The controller's answer to a poll. A missing job is represented by `None` at the client
/// boundary, not by an empty event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PollReply {
    Event(AgentEvent),
    Envelope { event: AgentEvent },
}

impl PollReply {
    pub fn into_event(self) -> AgentEvent {
        match self {
            Self::Event(event) | Self::Envelope { event } => event,
        }
    }
}

/// One log page. `offset` is the next offset to ask for; `state` is the job state if the
/// controller included it, which is how a follower learns that the job is terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogPage {
    pub data: String,
    pub offset: u64,
    pub state: Option<JobState>,
    pub terminal: bool,
}

impl Serialize for LogPage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut out = serializer.serialize_struct("LogPage", 4)?;
        out.serialize_field("data", &self.data)?;
        out.serialize_field("offset", &self.offset)?;
        out.serialize_field("state", &self.state)?;
        out.serialize_field("terminal", &self.terminal)?;
        out.end()
    }
}

impl<'de> Deserialize<'de> for LogPage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(log_page_from_value(value))
    }
}

fn log_page_from_value(value: serde_json::Value) -> LogPage {
    match value {
        serde_json::Value::String(data) => LogPage {
            data,
            ..LogPage::default()
        },
        serde_json::Value::Array(items) => LogPage {
            data: items
                .into_iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
            ..LogPage::default()
        },
        serde_json::Value::Object(mut map) => {
            let data = map
                .remove("data")
                .or_else(|| map.remove("log"))
                .or_else(|| map.remove("chunk"))
                .and_then(|value| match value {
                    serde_json::Value::String(text) => Some(text),
                    serde_json::Value::Array(items) => Some(
                        items
                            .into_iter()
                            .filter_map(|item| item.as_str().map(str::to_owned))
                            .collect(),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            let offset = ["next", "next_offset", "offset", "cursor"]
                .iter()
                .find_map(|key| map.get(*key).and_then(as_u64))
                .unwrap_or(0);
            let state = map
                .get("state")
                .or_else(|| map.get("job_state"))
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<JobState>().ok());
            let terminal = map
                .get("terminal")
                .or_else(|| map.get("done"))
                .or_else(|| map.get("eof"))
                .and_then(|value| value.as_bool())
                .unwrap_or_else(|| state.map(JobState::is_terminal).unwrap_or(false));
            LogPage {
                data,
                offset,
                state,
                terminal,
            }
        }
        _ => LogPage::default(),
    }
}

fn as_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// The body of a completion request. Kept here so callers do not build JSON by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionBody {
    pub job_id: JobId,
    pub epoch: u64,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<JobView>();
    check::<NodeView>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use lu_proto::{FailureClass, WorkspacePolicy, WorkspaceState};

    #[test]
    fn job_view_ignores_future_fields_and_keeps_known_ones() {
        let id = JobId::new();
        let json = format!(
            r#"{{"id":"{id}","state":"running","attempt":2,"created_at":10,
                "spec":{{"source": {{"repo": "/srv/repo", "ref": "main"}},"task":"go"}},
                "failure_class":"execution","future":{{"x":1}}}}"#
        );
        let view: JobView = serde_json::from_str(&json).unwrap();
        assert_eq!(view.id, id);
        assert_eq!(view.state, JobState::Running);
        assert_eq!(view.attempt, 2);
        assert_eq!(view.failure_class, Some(FailureClass::Execution));
        assert_eq!(view.spec.repo, "/srv/repo");
    }

    #[test]
    fn job_views_carry_dependencies() {
        let id = JobId::new();
        let dep = JobId::new();
        let json = format!(
            r#"{{"id":"{id}","state":"blocked","after":["{dep}"],"waiting_on":["{dep}"]}}"#
        );
        let view: JobView = serde_json::from_str(&json).unwrap();
        assert_eq!(view.state, JobState::Blocked);
        assert_eq!(view.after, vec![dep.clone()]);
        assert_eq!(view.waiting_on, vec![dep]);
    }

    #[test]
    fn job_views_carry_waiting_reasons_and_warnings() {
        let id = JobId::new();
        let json = format!(
            r#"{{"id":"{id}","state":"queued","waiting_reason":"no node with label php8.3","warnings":["no node with label php8.3"]}}"#
        );
        let view: JobView = serde_json::from_str(&json).unwrap();
        assert_eq!(
            view.waiting_reason.as_deref(),
            Some("no node with label php8.3")
        );
        assert_eq!(
            view.warnings(),
            &["no node with label php8.3".to_owned()][..]
        );
    }

    #[test]
    fn poll_requests_carry_node_labels() {
        let request = PollRequest {
            labels: vec!["gpu".to_owned()],
            ..PollRequest::default()
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"labels":["gpu"]}"#);
        let parsed: PollRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.labels, vec!["gpu"]);
    }

    #[test]
    fn poll_requests_carry_node_taints() {
        let request = PollRequest {
            taints: vec!["homelab".to_owned()],
            ..PollRequest::default()
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"taints":["homelab"]}"#);
        let parsed: PollRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.taints, vec!["homelab"]);
    }

    #[test]
    fn poll_requests_carry_workspace_heartbeat_fields() {
        let workspace = WorkspaceView {
            job_key: JobId::new().to_string(),
            state: WorkspaceState::Kept,
            bytes: 64,
            age_secs: 12,
            policy: WorkspacePolicy::Keep,
        };
        let beat = Heartbeat {
            node: NodeName::parse("node-a").unwrap(),
            harness: HarnessState::Busy,
            version: None,
            providers: Vec::new(),
            provider_models: Vec::new(),
            reiki: None,
            host: None,
            harness_ok: Some(true),
            disk_free_pct: Some(42),
            jobs: Vec::new(),
            labels: Vec::new(),
            label_sources: BTreeMap::new(),
            taints: Vec::new(),
            workspace_bytes: 64,
            workspace_count: 1,
            workspace_reclaimed_bytes_total: 128,
            workspace_evictions_total: 2,
            workspace_delete_errors_total: 3,
            gc_removed_total: 2,
            gc_bytes_freed_total: 64,
            workspaces: vec![workspace.clone()],
        };
        let request = PollRequest::from(beat);
        assert_eq!(request.harness_ok, Some(true));
        assert_eq!(request.disk_free_pct, Some(42));
        assert_eq!(request.workspace_bytes, 64);
        assert_eq!(request.workspace_count, 1);
        assert_eq!(request.workspace_reclaimed_bytes_total, 128);
        assert_eq!(request.workspace_evictions_total, 2);
        assert_eq!(request.workspace_delete_errors_total, 3);
        assert_eq!(request.gc_removed_total, 2);
        assert_eq!(request.gc_bytes_freed_total, 64);
        assert_eq!(request.workspaces, vec![workspace]);
        let json = serde_json::to_string(&request).unwrap();
        let parsed: PollRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn poll_requests_carry_provider_model_pairs() {
        let request = PollRequest {
            provider_models: vec![ProviderModel {
                provider: "codex".to_owned(),
                model: "gpt-5".to_owned(),
            }],
            ..PollRequest::default()
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"provider_models":[{"provider":"codex","model":"gpt-5"}]}"#
        );
        let parsed: PollRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.provider_models, request.provider_models);
    }

    #[test]
    fn a_minimal_job_view_still_deserialises() {
        let id = JobId::new();
        let json = format!(r#"{{"id":"{id}","state":"queued"}}"#);
        let view: JobView = serde_json::from_str(&json).unwrap();
        assert_eq!(view.attempt, 0);
        assert!(view.node.is_none());
    }

    #[test]
    fn log_pages_accept_the_shapes_a_controller_may_send() {
        let page: LogPage =
            serde_json::from_str(r#"{"data":"hi","next":7,"state":"running"}"#).unwrap();
        assert_eq!(page.data, "hi");
        assert_eq!(page.offset, 7);
        assert_eq!(page.state, Some(JobState::Running));
        assert!(!page.terminal);

        let page: LogPage =
            serde_json::from_str(r#"{"data":"done","offset":9,"state":"done"}"#).unwrap();
        assert_eq!(page.offset, 9);
        assert!(page.terminal);

        let page: LogPage = serde_json::from_str(r#""plain""#).unwrap();
        assert_eq!(page.data, "plain");
    }

    #[test]
    fn poll_requests_serialise_without_empty_options() {
        let request = PollRequest::default();
        assert_eq!(serde_json::to_string(&request).unwrap(), "{}");
    }

    #[test]
    fn node_views_carry_the_agent_version() {
        let node: NodeView = serde_json::from_str(
            r#"{"name":"node-a","version":"0.1.10","harness":"idle","draining":false}"#,
        )
        .unwrap();
        assert_eq!(node.version.as_deref(), Some("0.1.10"));

        let node: NodeView =
            serde_json::from_str(r#"{"name":"node-a","labels":["gpu","php8.3"]}"#).unwrap();
        assert_eq!(node.labels, vec!["gpu".to_owned(), "php8.3".to_owned()]);

        let node: NodeView =
            serde_json::from_str(r#"{"name":"node-a","labels":["gpu"],"taints":["homelab"]}"#)
                .unwrap();
        assert_eq!(node.taints, vec!["homelab".to_owned()]);
    }

    #[test]
    fn node_views_carry_provider_model_pairs() {
        let node: NodeView = serde_json::from_str(
            r#"{"name":"node-a","provider_models":[{"provider":"codex","model":"gpt-5"}]}"#,
        )
        .unwrap();
        assert_eq!(
            node.provider_models,
            vec![ProviderModel {
                provider: "codex".to_owned(),
                model: "gpt-5".to_owned(),
            }]
        );
        let node: NodeView = serde_json::from_str(r#"{"name":"node-a"}"#).unwrap();
        assert!(node.provider_models.is_empty());
    }

    #[test]
    fn node_views_carry_probe_label_sources() {
        let node: NodeView = serde_json::from_str(
            r#"{"name":"node-a","labels":["gpu"],"label_sources":{"gpu":"probed"}}"#,
        )
        .unwrap();
        assert_eq!(
            node.label_sources.get("gpu").map(String::as_str),
            Some(lu_proto::LABEL_SOURCE_PROBED)
        );
        let node: NodeView = serde_json::from_str(r#"{"name":"node-a"}"#).unwrap();
        assert!(node.label_sources.is_empty());
    }

    #[test]
    fn status_views_ignore_future_fields() {
        let status: StatusView = serde_json::from_str(
            r#"{"version":"0.1.10","uptime_secs":9,"schema_version":2,"db_bytes":4096,
                "recovered_on_boot":1,"jobs":{"queued":2,"running":1},
                "oldest_queued_secs":7,"nodes":3,"nodes_online":2,"lease_secs":60,
                "future":true}"#,
        )
        .unwrap();
        assert_eq!(status.version, "0.1.10");
        assert_eq!(status.jobs.get("queued"), Some(&2));
        assert_eq!(status.nodes_online, 2);
        assert_eq!(status.lease_secs, 60);
    }

    #[test]
    fn node_views_read_the_draining_field() {
        let node: NodeView =
            serde_json::from_str(r#"{"name":"node-a","draining":true,"harness":"idle"}"#).unwrap();
        assert!(node.draining);
    }
}
