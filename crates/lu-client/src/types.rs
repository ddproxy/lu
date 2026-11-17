//! Request and response types for the controller API.
//!
//! The controller is the source of these strings, so display code must pass them through
//! [`lu_proto::printable`]; the types do not pretend to sanitise untrusted text on construction.

use lu_proto::{
    AgentEvent, FailureClass, HarnessState, Heartbeat, JobId, JobSpec, JobState, NodeName, Outcome,
    ProviderStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use lu_proto::{HostStats, JobStats, NodeStats, ProcessStats};

/// Largest JSON response the client will read. Artifacts have their own per-file cap.
pub const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;

/// A submitted job, as the controller sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub id: JobId,
    pub state: JobState,
    #[serde(default = "empty_spec")]
    pub spec: JobSpec,
    #[serde(default)]
    pub node: Option<NodeName>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, alias = "lease_epoch", alias = "fencing_epoch")]
    pub epoch: u64,
    #[serde(default, alias = "created", alias = "created_at_ms")]
    pub created_at: u64,
    #[serde(default, alias = "started", alias = "started_at_ms")]
    pub started_at: Option<u64>,
    #[serde(default, alias = "finished", alias = "finished_at_ms")]
    pub finished_at: Option<u64>,
    #[serde(default, alias = "failure")]
    pub failure_class: Option<FailureClass>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub after: Vec<JobId>,
    #[serde(default)]
    pub waiting_on: Vec<JobId>,
}

impl JobView {
    /// A view for a route that answered 204 (cancel) or with only an id (submit).
    pub fn placeholder(id: JobId, state: JobState) -> Self {
        Self {
            id,
            state,
            spec: empty_spec(),
            node: None,
            attempt: 0,
            epoch: 0,
            created_at: 0,
            started_at: None,
            finished_at: None,
            failure_class: None,
            summary: None,
            after: Vec::new(),
            waiting_on: Vec::new(),
        }
    }
}

/// A page of jobs. The controller may add fields later; unknown ones are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListJobs {
    #[serde(default)]
    pub jobs: Vec<JobView>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub next: Option<String>,
}

/// One node's heartbeat and cordon state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    #[serde(alias = "node")]
    pub name: NodeName,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub harness: Option<HarnessState>,
    #[serde(default)]
    pub providers: Vec<ProviderStatus>,
    #[serde(default, alias = "heartbeat_at", alias = "last_seen_at")]
    pub last_seen: u64,
    #[serde(default, alias = "drained", alias = "draining")]
    pub cordoned: bool,
    #[serde(default)]
    pub load1: Option<f64>,
    #[serde(default)]
    pub mem_available_kb: Option<u64>,
    #[serde(default)]
    pub mem_total_kb: Option<u64>,
    #[serde(default)]
    pub stuck: u64,
}

impl NodeView {
    /// A view for a drain route that answered 204.
    pub fn placeholder(name: NodeName, cordoned: bool) -> Self {
        Self {
            name,
            version: None,
            harness: None,
            providers: Vec::new(),
            last_seen: 0,
            cordoned,
            load1: None,
            mem_available_kb: None,
            mem_total_kb: None,
            stuck: 0,
        }
    }
}

/// The node list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListNodes {
    #[serde(default)]
    pub nodes: Vec<NodeView>,
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
    #[serde(default)]
    pub jobs: BTreeMap<String, u64>,
    #[serde(default)]
    pub oldest_queued_secs: u64,
    #[serde(default)]
    pub nodes: u64,
    #[serde(default)]
    pub nodes_online: u64,
    #[serde(default)]
    pub lease_secs: u64,
}

/// What an agent sends when it asks for work. The token already identifies the node, so `node`
/// is optional; it is included when an agent wants the controller to verify it explicitly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PollRequest {
    #[serde(alias = "name", skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reiki: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
    #[serde(alias = "wait", skip_serializing_if = "Option::is_none")]
    pub wait_secs: Option<u64>,
}

impl From<Heartbeat> for PollRequest {
    fn from(beat: Heartbeat) -> Self {
        Self {
            node: Some(beat.node),
            harness: Some(beat.harness),
            version: beat.version,
            providers: beat.providers,
            reiki: beat.reiki,
            host: beat.host,
            jobs: beat.jobs,
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
                .and_then(|value| serde_json::from_value::<JobState>(value.clone()).ok());
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

/// A small envelope used by some controllers for single-object replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEnvelope {
    pub job: JobView,
}

fn empty_spec() -> JobSpec {
    JobSpec {
        repo: String::new(),
        git_ref: String::new(),
        task: String::new(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: lu_proto::DEFAULT_TIMEOUT_SECS,
        idle_timeout_secs: None,
        idempotency_key: None,
        after: Vec::new(),
    }
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

    #[test]
    fn job_view_ignores_future_fields_and_keeps_known_ones() {
        let id = JobId::new();
        let json = format!(
            r#"{{"id":"{id}","state":"running","attempt":2,"created_at":10,
                "spec":{{"repo":"/srv/repo","git_ref":"main","task":"go"}},
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
    fn node_views_accept_drained_as_an_alias() {
        let node: NodeView =
            serde_json::from_str(r#"{"name":"node-a","drained":true,"harness":"idle"}"#).unwrap();
        assert_eq!(node.name.as_str(), "node-a");
        assert!(node.cordoned);
    }

    #[test]
    fn node_views_carry_the_agent_version() {
        let node: NodeView = serde_json::from_str(
            r#"{"name":"node-a","version":"0.1.10","harness":"idle","draining":false}"#,
        )
        .unwrap();
        assert_eq!(node.version.as_deref(), Some("0.1.10"));
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
    fn node_views_accept_the_controllers_draining_field() {
        let node: NodeView =
            serde_json::from_str(r#"{"name":"node-a","draining":true,"harness":"idle"}"#).unwrap();
        assert!(node.cordoned);
    }
}
