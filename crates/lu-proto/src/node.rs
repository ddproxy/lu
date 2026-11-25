//! Node heartbeat: what a peer says about itself. The forwarded provider-health document is opaque
//! and capped, and provider names are checked with the same rule as node names.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::{is_name_byte, JobId, NodeName};
use crate::label::{valid_label, LABEL_SOURCE_PROBED};
use crate::limits::{
    MAX_LABEL_BYTES, MAX_MODEL_BYTES, MAX_NAME_BYTES, MAX_NODE_LABELS, MAX_PROVIDERS,
    MAX_PROVIDER_HEALTH_BYTES, MAX_STATS_BYTES, MAX_TAINTS, MAX_VERSION_BYTES, MAX_WORKSPACES,
};
use crate::view::WorkspaceView;

/// Why a string was not a recognised harness state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseHarnessStateError;

impl fmt::Display for ParseHarnessStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a recognised harness state")
    }
}

impl std::error::Error for ParseHarnessStateError {}

/// Whether the peer's harness is free to take a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessState {
    Idle,
    Busy,
}

impl HarnessState {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
        }
    }
}

impl fmt::Display for HarnessState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessState {
    type Err = ParseHarnessStateError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(match text {
            "idle" => Self::Idle,
            "busy" => Self::Busy,
            _ => return Err(ParseHarnessStateError),
        })
    }
}

/// Why a string was not a recognised provider state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseProviderStateError;

impl fmt::Display for ParseProviderStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a recognised provider state")
    }
}

impl std::error::Error for ParseProviderStateError {}

/// A provider's availability on this peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Ok,
    Cooling,
    Disabled,
    Unknown,
}

impl ProviderState {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Cooling => "cooling",
            Self::Disabled => "disabled",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ProviderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderState {
    type Err = ParseProviderStateError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(match text {
            "ok" => Self::Ok,
            "cooling" => Self::Cooling,
            "disabled" => Self::Disabled,
            "unknown" => Self::Unknown,
            _ => return Err(ParseProviderStateError),
        })
    }
}

/// What the agent is doing with one job's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// The harness is still using the tree.
    Running,
    /// Waiting for the controller to confirm the artifacts are stored and dependants are done.
    Waiting,
    /// Kept until its TTL expires or an eviction reclaims it.
    Kept,
    /// A deletion is queued or being retried.
    Deleting,
}

impl WorkspaceState {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Kept => "kept",
            Self::Deleting => "deleting",
        }
    }
}

impl fmt::Display for WorkspaceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One provider and its state, as reported by the peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub name: String,
    pub state: ProviderState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soonest_secs: Option<u64>,
}

/// A provider and the model the agent would run for it, offered in preference order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub provider: String,
    pub model: String,
}

/// The host numbers sampled from `/proc`. The disk fields are optional because a portable
/// answer is not available without a platform filesystem call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostStats {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub swap_used_kb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    pub uptime_secs: u64,
}

/// One process in a job's harness tree, as sampled from `/proc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessStats {
    pub pid: u32,
    pub ppid: u32,
    pub state: String,
    pub cpu_secs: u64,
    pub rss_kb: u64,
    pub age_secs: u64,
    pub cmdline: String,
    pub open_files: u32,
}

/// One running job and the processes in its harness process group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStats {
    pub job_id: JobId,
    pub pgid: u32,
    pub output_idle_secs: u64,
    pub cpu_idle_secs: u64,
    pub stuck: bool,
    pub processes: Vec<ProcessStats>,
}

/// The stats a node reports: its host numbers and its running jobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub label_sources: BTreeMap<String, String>,
    /// Bytes in workspaces currently on disk.
    #[serde(default)]
    pub workspace_bytes: u64,
    /// Workspaces currently on disk.
    #[serde(default)]
    pub workspace_count: u64,
    /// Bytes reclaimed by workspace deletion since the agent started.
    #[serde(default)]
    pub workspace_reclaimed_bytes_total: u64,
    /// Workspaces removed by high-water or size-cap eviction.
    #[serde(default)]
    pub workspace_evictions_total: u64,
    /// Failed deletion attempts; failures are retried and never silently ignored.
    #[serde(default)]
    pub workspace_delete_errors_total: u64,
    /// Scratch directories removed by the agent's housekeeping since it started.
    #[serde(default)]
    pub gc_removed_total: u64,
    /// Bytes freed by the agent's housekeeping since it started.
    #[serde(default)]
    pub gc_bytes_freed_total: u64,
    /// Kept and pending workspaces, oldest first, as last reported by the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceView>,
}

/// Default minimum free work-filesystem space, in percent, before a node may take work.
pub const DEFAULT_MIN_DISK_FREE_PCT: u8 = 8;

/// A periodic message from an agent. Receivers ignore fields they do not know, so an agent may be
/// newer than its controller; only add optional fields. The provider-health field is an opaque
/// document, forwarded unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heartbeat {
    pub node: NodeName,
    pub harness: HarnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    /// Provider/model pairs this node offers to provider-agnostic jobs, in preference order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_models: Vec<ProviderModel>,
    #[serde(default, rename = "reiki", skip_serializing_if = "Option::is_none")]
    pub provider_health: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    /// False when the agent's harness is unhealthy. Absent means healthy for older agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_ok: Option<bool>,
    /// Free space on the work filesystem as a percentage. Absent means no health signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Additive description of where labels came from; `probed` marks an operator-declared probe.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub label_sources: BTreeMap<String, String>,
    /// Taints a job must tolerate before it may run here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub taints: Vec<String>,
    /// Bytes in workspaces currently on disk.
    #[serde(default)]
    pub workspace_bytes: u64,
    /// Workspaces currently on disk.
    #[serde(default)]
    pub workspace_count: u64,
    /// Bytes reclaimed by workspace deletion since the agent started.
    #[serde(default)]
    pub workspace_reclaimed_bytes_total: u64,
    /// Workspaces removed by high-water or size-cap eviction.
    #[serde(default)]
    pub workspace_evictions_total: u64,
    /// Failed deletion attempts; failures are retried and never silently ignored.
    #[serde(default)]
    pub workspace_delete_errors_total: u64,
    /// Scratch directories removed by housekeeping since the agent started.
    #[serde(default)]
    pub gc_removed_total: u64,
    /// Bytes freed by housekeeping since the agent started.
    #[serde(default)]
    pub gc_bytes_freed_total: u64,
    /// Workspaces the agent is tracking, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceView>,
}

#[derive(Deserialize)]
// Unknown fields are accepted so an agent from a newer release can talk to this controller;
// see docs/compatibility.md.
struct RawHeartbeat {
    pub node: NodeName,
    pub harness: HarnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    /// Provider/model pairs this node offers to provider-agnostic jobs, in preference order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_models: Vec<ProviderModel>,
    #[serde(default, rename = "reiki", skip_serializing_if = "Option::is_none")]
    pub provider_health: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    /// False when the agent's harness is unhealthy. Absent means healthy for older agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_ok: Option<bool>,
    /// Free space on the work filesystem as a percentage. Absent means no health signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Additive description of where labels came from; `probed` marks an operator-declared probe.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub label_sources: BTreeMap<String, String>,
    /// Taints a job must tolerate before it may run here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub taints: Vec<String>,
    #[serde(default)]
    pub workspace_bytes: u64,
    #[serde(default)]
    pub workspace_count: u64,
    #[serde(default)]
    pub workspace_reclaimed_bytes_total: u64,
    #[serde(default)]
    pub workspace_evictions_total: u64,
    #[serde(default)]
    pub workspace_delete_errors_total: u64,
    #[serde(default)]
    pub gc_removed_total: u64,
    #[serde(default)]
    pub gc_bytes_freed_total: u64,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceView>,
}

impl TryFrom<RawHeartbeat> for Heartbeat {
    type Error = HeartbeatError;
    fn try_from(raw: RawHeartbeat) -> Result<Self, Self::Error> {
        let mut hb = Heartbeat {
            node: raw.node,
            harness: raw.harness,
            version: raw.version,
            providers: raw.providers,
            provider_models: raw.provider_models,
            provider_health: raw.provider_health,
            host: raw.host,
            harness_ok: raw.harness_ok,
            disk_free_pct: raw.disk_free_pct,
            jobs: raw.jobs,
            labels: raw.labels,
            label_sources: raw.label_sources,
            taints: raw.taints,
            workspace_bytes: raw.workspace_bytes,
            workspace_count: raw.workspace_count,
            workspace_reclaimed_bytes_total: raw.workspace_reclaimed_bytes_total,
            workspace_evictions_total: raw.workspace_evictions_total,
            workspace_delete_errors_total: raw.workspace_delete_errors_total,
            gc_removed_total: raw.gc_removed_total,
            gc_bytes_freed_total: raw.gc_bytes_freed_total,
            workspaces: raw.workspaces,
        };
        hb.trim_stats();
        hb.validate()?;
        Ok(hb)
    }
}

impl<'de> Deserialize<'de> for Heartbeat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawHeartbeat::deserialize(deserializer)?;
        Heartbeat::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl Heartbeat {
    /// Drops process entries first, then whole jobs, until the serialized stats fit the cap.
    /// The order removes the least busy processes first within each job, because a sampler
    /// lists processes busiest-first and this pops from the end of the list.
    pub fn trim_stats(&mut self) {
        while self.stats_len() > MAX_STATS_BYTES {
            let mut dropped_process = false;
            for job in self.jobs.iter_mut().rev() {
                if job.processes.pop().is_some() {
                    dropped_process = true;
                    break;
                }
            }
            if !dropped_process && self.jobs.pop().is_none() {
                break;
            }
        }
    }

    fn stats_len(&self) -> usize {
        if self.host.is_none() && self.jobs.is_empty() {
            return 0;
        }
        let stats = NodeStats {
            host: self.host.clone(),
            jobs: self.jobs.clone(),
            labels: Vec::new(),
            label_sources: BTreeMap::new(),
            // The counters are a handful of numbers; the listing is bounded separately.
            workspace_bytes: self.workspace_bytes,
            workspace_count: self.workspace_count,
            workspace_reclaimed_bytes_total: self.workspace_reclaimed_bytes_total,
            workspace_evictions_total: self.workspace_evictions_total,
            workspace_delete_errors_total: self.workspace_delete_errors_total,
            gc_removed_total: self.gc_removed_total,
            gc_bytes_freed_total: self.gc_bytes_freed_total,
            workspaces: Vec::new(),
        };
        serde_json::to_vec(&stats)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    }

    /// Checks the caps a single heartbeat must respect.
    pub fn validate(&self) -> Result<(), HeartbeatError> {
        if let Some(value) = self.disk_free_pct {
            if value > 100 {
                return Err(HeartbeatError::DiskFreePct { value });
            }
        }
        if let Some(version) = &self.version {
            if version.is_empty()
                || version.len() > MAX_VERSION_BYTES
                || !version
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ')
            {
                return Err(HeartbeatError::Version {
                    value: version.clone(),
                });
            }
        }
        if self.providers.len() > MAX_PROVIDERS {
            return Err(HeartbeatError::TooManyProviders {
                got: self.providers.len(),
            });
        }
        for (index, provider) in self.providers.iter().enumerate() {
            if !valid_provider_name(&provider.name) {
                return Err(HeartbeatError::ProviderName {
                    index,
                    name: provider.name.clone(),
                });
            }
        }
        if self.provider_models.len() > MAX_PROVIDERS {
            return Err(HeartbeatError::TooManyProviderModels {
                got: self.provider_models.len(),
            });
        }
        for (index, pair) in self.provider_models.iter().enumerate() {
            if !valid_provider_name(&pair.provider) {
                return Err(HeartbeatError::ProviderModelName {
                    index,
                    name: pair.provider.clone(),
                });
            }
            if pair.model.is_empty() || pair.model.len() > MAX_MODEL_BYTES {
                return Err(HeartbeatError::ProviderModelModel {
                    index,
                    model: pair.model.clone(),
                });
            }
            if self.provider_models[..index]
                .iter()
                .any(|previous| previous.provider == pair.provider)
            {
                return Err(HeartbeatError::DuplicateProviderModel {
                    index,
                    name: pair.provider.clone(),
                });
            }
        }
        if self.labels.len() > MAX_NODE_LABELS {
            return Err(HeartbeatError::TooManyLabels {
                got: self.labels.len(),
            });
        }
        for label in &self.labels {
            if !valid_label(label) {
                return Err(HeartbeatError::Label {
                    value: label.clone(),
                });
            }
        }
        if self.label_sources.len() > MAX_NODE_LABELS {
            return Err(HeartbeatError::TooManyLabelSources {
                got: self.label_sources.len(),
            });
        }
        for (label, source) in &self.label_sources {
            if !valid_label(label) || source != LABEL_SOURCE_PROBED || !self.labels.contains(label)
            {
                return Err(HeartbeatError::LabelSource {
                    label: label.clone(),
                    source: source.clone(),
                });
            }
        }
        if self.taints.len() > MAX_TAINTS {
            return Err(HeartbeatError::TooManyTaints {
                got: self.taints.len(),
            });
        }
        for taint in &self.taints {
            if !valid_label(taint) {
                return Err(HeartbeatError::Taint {
                    value: taint.clone(),
                });
            }
        }
        if self.workspaces.len() > MAX_WORKSPACES {
            return Err(HeartbeatError::TooManyWorkspaces {
                got: self.workspaces.len(),
            });
        }
        for (index, workspace) in self.workspaces.iter().enumerate() {
            if JobId::parse(&workspace.job_key).is_err() {
                return Err(HeartbeatError::WorkspaceJobKey {
                    index,
                    key: workspace.job_key.clone(),
                });
            }
        }
        if let Some(provider_health) = &self.provider_health {
            let size = serde_json::to_vec(provider_health)
                .map_err(|_| HeartbeatError::ProviderHealthNotSerializable)?
                .len();
            if size > MAX_PROVIDER_HEALTH_BYTES {
                return Err(HeartbeatError::ProviderHealthTooLarge { got: size });
            }
        }
        Ok(())
    }
}

/// Why a heartbeat was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatError {
    DiskFreePct { value: u8 },
    Version { value: String },
    TooManyProviders { got: usize },
    ProviderName { index: usize, name: String },
    TooManyProviderModels { got: usize },
    ProviderModelName { index: usize, name: String },
    ProviderModelModel { index: usize, model: String },
    DuplicateProviderModel { index: usize, name: String },
    TooManyLabels { got: usize },
    Label { value: String },
    TooManyLabelSources { got: usize },
    LabelSource { label: String, source: String },
    TooManyTaints { got: usize },
    Taint { value: String },
    TooManyWorkspaces { got: usize },
    WorkspaceJobKey { index: usize, key: String },
    ProviderHealthTooLarge { got: usize },
    ProviderHealthNotSerializable,
}

impl std::fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DiskFreePct { value } => {
                write!(f, "disk_free_pct must be 0 to 100 (got {value})")
            }
            Self::Version { value } => write!(
                f,
                "the agent version {value:?} must be 1 to {MAX_VERSION_BYTES} printable ASCII characters"
            ),
            Self::TooManyProviders { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_PROVIDERS} providers (got {got})"
            ),
            Self::ProviderName { index, name } => write!(
                f,
                "provider #{index} has an invalid name {name:?}; it must be 1 to {MAX_NAME_BYTES} characters from A-Z a-z 0-9 . _ -"
            ),
            Self::TooManyProviderModels { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_PROVIDERS} provider/model pairs (got {got})"
            ),
            Self::ProviderModelName { index, name } => write!(
                f,
                "provider/model pair #{index} has an invalid provider {name:?}; it must be 1 to {MAX_NAME_BYTES} characters from A-Z a-z 0-9 . _ -"
            ),
            Self::ProviderModelModel { index, model } => write!(
                f,
                "provider/model pair #{index} has an invalid model {model:?}; it must be 1 to {MAX_MODEL_BYTES} characters"
            ),
            Self::DuplicateProviderModel { index, name } => write!(
                f,
                "provider/model pair #{index} repeats provider {name:?}"
            ),
            Self::TooManyLabels { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_NODE_LABELS} labels (got {got})"
            ),
            Self::Label { value } => write!(
                f,
                "invalid node label {value:?}; it must be 1 to {MAX_LABEL_BYTES} characters from A-Z a-z 0-9 . _ - / with at most one '='"
            ),
            Self::TooManyLabelSources { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_NODE_LABELS} label sources (got {got})"
            ),
            Self::LabelSource { label, source } => write!(
                f,
                "invalid source {source:?} for label {label:?}; only {LABEL_SOURCE_PROBED:?} is allowed and the label must also appear in labels"
            ),
            Self::TooManyTaints { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_TAINTS} taints (got {got})"
            ),
            Self::Taint { value } => write!(
                f,
                "invalid node taint {value:?}; it must be 1 to {MAX_LABEL_BYTES} characters from A-Z a-z 0-9 . _ - / with at most one '='"
            ),
            Self::TooManyWorkspaces { got } => write!(
                f,
                "a heartbeat may list at most {MAX_WORKSPACES} workspaces (got {got})"
            ),
            Self::WorkspaceJobKey { index, key } => write!(
                f,
                "workspace #{index} has an invalid job key {key:?}"
            ),
            Self::ProviderHealthTooLarge { got } => write!(
                f,
                "the forwarded provider-health document is larger than {MAX_PROVIDER_HEALTH_BYTES} bytes (got {got})"
            ),
            Self::ProviderHealthNotSerializable => {
                f.write_str("the forwarded provider-health document could not be serialised")
            }
        }
    }
}

impl std::error::Error for HeartbeatError {}

/// Whether `name` may appear as a provider in a heartbeat.
pub fn valid_provider_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && name.bytes().all(is_name_byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn heartbeat() -> Heartbeat {
        Heartbeat {
            node: NodeName::from_str("peer-a").unwrap(),
            harness: HarnessState::Idle,
            version: None,
            providers: vec![ProviderStatus {
                name: "example-provider".to_owned(),
                state: ProviderState::Ok,
                soonest_secs: None,
            }],
            provider_models: Vec::new(),
            provider_health: None,
            host: None,
            harness_ok: None,
            disk_free_pct: None,
            jobs: Vec::new(),
            labels: Vec::new(),
            label_sources: BTreeMap::new(),
            taints: Vec::new(),
            workspace_bytes: 0,
            workspace_count: 0,
            workspace_reclaimed_bytes_total: 0,
            workspace_evictions_total: 0,
            workspace_delete_errors_total: 0,
            gc_removed_total: 0,
            gc_bytes_freed_total: 0,
            workspaces: Vec::new(),
        }
    }

    fn host() -> HostStats {
        HostStats {
            load1: 1.5,
            load5: 1.25,
            load15: 1.0,
            mem_total_kb: 16_384,
            mem_available_kb: 8_192,
            swap_used_kb: 0,
            disk_free_bytes: Some(1_000_000),
            disk_total_bytes: Some(8_000_000),
            uptime_secs: 42,
        }
    }

    fn job(job_id: &str) -> JobStats {
        JobStats {
            job_id: JobId::from_str(job_id).unwrap(),
            pgid: 123,
            output_idle_secs: 301,
            cpu_idle_secs: 302,
            stuck: true,
            processes: vec![ProcessStats {
                pid: 123,
                ppid: 1,
                state: "R".to_owned(),
                cpu_secs: 7,
                rss_kb: 2048,
                age_secs: 30,
                cmdline: "opencode run".to_owned(),
                open_files: 12,
            }],
        }
    }

    #[test]
    fn a_heartbeat_round_trips() {
        let mut beat = heartbeat();
        beat.harness = HarnessState::Busy;
        beat.providers[0].state = ProviderState::Cooling;
        beat.providers[0].soonest_secs = Some(30);
        beat.provider_health = Some(serde_json::json!({"status": "ok"}));
        assert!(beat.validate().is_ok());
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, beat);
    }

    #[test]
    fn providers_default_to_empty_and_are_omitted() {
        let json = r#"{"node":"peer-a","harness":"idle"}"#;
        let beat: Heartbeat = serde_json::from_str(json).unwrap();
        assert!(beat.providers.is_empty());
        assert!(beat.provider_health.is_none());
        assert!(beat.validate().is_ok());
        let out = serde_json::to_string(&beat).unwrap();
        assert!(!out.contains("providers"), "{out}");
        assert!(!out.contains("reiki"), "{out}");
    }

    #[test]
    fn an_agent_version_is_optional_and_checked() {
        let mut beat = heartbeat();
        beat.version = Some("0.1.10".to_owned());
        assert!(beat.validate().is_ok());
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version.as_deref(), Some("0.1.10"));

        for version in ["0.6.2-dev.1.gc08a5a0", "0.6.2-dev.1+gc08a5a0"] {
            beat.version = Some(version.to_owned());
            assert!(beat.validate().is_ok(), "{version}");
            let json = serde_json::to_string(&beat).unwrap();
            let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.version.as_deref(), Some(version));
        }

        for version in [
            String::new(),
            "has\nnewline".to_owned(),
            "café".to_owned(),
            "a".repeat(MAX_VERSION_BYTES + 1),
        ] {
            beat.version = Some(version);
            assert!(matches!(
                beat.validate().unwrap_err(),
                HeartbeatError::Version { .. }
            ));
        }
        beat.version = Some("a".repeat(MAX_VERSION_BYTES));
        assert!(beat.validate().is_ok());
    }

    #[test]
    fn too_many_providers_are_refused() {
        let mut beat = heartbeat();
        beat.providers = (0..=MAX_PROVIDERS)
            .map(|index| ProviderStatus {
                name: format!("provider-{index}"),
                state: ProviderState::Unknown,
                soonest_secs: None,
            })
            .collect();
        assert_eq!(
            beat.validate().unwrap_err(),
            HeartbeatError::TooManyProviders {
                got: MAX_PROVIDERS + 1
            }
        );
    }

    #[test]
    fn a_bad_provider_name_is_refused() {
        for name in ["", "bad name", "peer/../a", "café", "peer\u{1b}[31m"] {
            let mut beat = heartbeat();
            beat.providers[0].name = name.to_owned();
            assert!(matches!(
                beat.validate().unwrap_err(),
                HeartbeatError::ProviderName { .. }
            ));
        }
        let mut beat = heartbeat();
        beat.providers[0].name = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::ProviderName { .. }
        ));
    }

    #[test]
    fn node_labels_round_trip_and_are_omitted_when_empty() {
        let mut beat = heartbeat();
        assert!(!serde_json::to_string(&beat).unwrap().contains("labels"));
        beat.labels = vec!["php8.3".to_owned(), "region=eu".to_owned()];
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.labels, beat.labels);
        assert!(beat.validate().is_ok());
    }

    #[test]
    fn probed_label_sources_round_trip_and_are_omitted_when_empty() {
        let mut beat = heartbeat();
        assert!(!serde_json::to_string(&beat)
            .unwrap()
            .contains("label_sources"));
        beat.labels = vec!["gpu".to_owned()];
        beat.label_sources = BTreeMap::from([("gpu".to_owned(), LABEL_SOURCE_PROBED.to_owned())]);
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, beat);
        assert!(beat.validate().is_ok());
    }

    #[test]
    fn label_sources_must_be_known_probed_labels() {
        let mut beat = heartbeat();
        beat.labels = vec!["gpu".to_owned()];
        beat.label_sources = BTreeMap::from([("gpu".to_owned(), "fixed".to_owned())]);
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::LabelSource { .. }
        ));
        beat.label_sources.clear();
        beat.label_sources
            .insert("ghost".to_owned(), LABEL_SOURCE_PROBED.to_owned());
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::LabelSource { .. }
        ));
        beat.labels = (0..MAX_NODE_LABELS).map(|i| format!("label-{i}")).collect();
        beat.label_sources = beat
            .labels
            .iter()
            .cloned()
            .map(|label| (label, LABEL_SOURCE_PROBED.to_owned()))
            .collect();
        beat.label_sources
            .insert("extra".to_owned(), LABEL_SOURCE_PROBED.to_owned());
        assert_eq!(
            beat.validate().unwrap_err(),
            HeartbeatError::TooManyLabelSources {
                got: MAX_NODE_LABELS + 1
            }
        );
    }

    #[test]
    fn too_many_or_invalid_node_labels_are_refused() {
        let mut beat = heartbeat();
        beat.labels = (0..=MAX_NODE_LABELS)
            .map(|i| format!("label-{i}"))
            .collect();
        assert_eq!(
            beat.validate().unwrap_err(),
            HeartbeatError::TooManyLabels {
                got: MAX_NODE_LABELS + 1
            }
        );
        for label in [
            "",
            "bad label",
            "café",
            "a=b=c",
            &"a".repeat(MAX_LABEL_BYTES + 1),
        ] {
            beat.labels = vec![label.to_owned()];
            assert!(
                matches!(beat.validate().unwrap_err(), HeartbeatError::Label { .. }),
                "{label:?}"
            );
        }
    }

    #[test]
    fn node_taints_round_trip_and_are_omitted_when_empty() {
        let mut beat = heartbeat();
        assert!(!serde_json::to_string(&beat).unwrap().contains("taints"));
        beat.taints = vec!["lab".to_owned(), "region=eu".to_owned()];
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.taints, beat.taints);
        assert!(beat.validate().is_ok());
    }

    #[test]
    fn too_many_or_invalid_node_taints_are_refused() {
        let mut beat = heartbeat();
        beat.taints = (0..=MAX_TAINTS).map(|i| format!("taint-{i}")).collect();
        assert_eq!(
            beat.validate().unwrap_err(),
            HeartbeatError::TooManyTaints {
                got: MAX_TAINTS + 1
            }
        );
        for taint in [
            "",
            "bad taint",
            "café",
            "a=b=c",
            &"a".repeat(MAX_LABEL_BYTES + 1),
        ] {
            beat.taints = vec![taint.to_owned()];
            assert!(
                matches!(beat.validate().unwrap_err(), HeartbeatError::Taint { .. }),
                "{taint:?}"
            );
        }
    }

    #[test]
    fn old_heartbeats_without_labels_still_parse() {
        let beat: Heartbeat =
            serde_json::from_str(r#"{"node":"peer-a","harness":"idle","providers":[]}"#).unwrap();
        assert!(beat.labels.is_empty());
    }

    #[test]
    fn old_heartbeats_without_label_sources_still_parse() {
        let beat: Heartbeat =
            serde_json::from_str(r#"{"node":"peer-a","harness":"idle","labels":["gpu"]}"#).unwrap();
        assert!(beat.label_sources.is_empty());
        assert!(!serde_json::to_string(&beat)
            .unwrap()
            .contains("label_sources"));
    }

    #[test]
    fn older_heartbeats_without_taints_still_parse() {
        let beat: Heartbeat =
            serde_json::from_str(r#"{"node":"peer-a","harness":"idle","labels":["gpu"]}"#).unwrap();
        assert_eq!(beat.labels, vec!["gpu".to_owned()]);
        assert!(beat.taints.is_empty());
        let out = serde_json::to_string(&beat).unwrap();
        assert!(!out.contains("taints"), "{out}");
    }

    #[test]
    fn an_oversized_provider_health_document_is_refused() {
        let mut beat = heartbeat();
        beat.provider_health = Some(serde_json::Value::String(
            "a".repeat(MAX_PROVIDER_HEALTH_BYTES),
        ));
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::ProviderHealthTooLarge { .. }
        ));
    }

    #[test]
    fn host_and_job_stats_round_trip() {
        let mut beat = heartbeat();
        beat.host = Some(host());
        beat.jobs = vec![job("01ARZ3NDEKTSV4RRFFQ69G5FAV")];
        assert!(beat.validate().is_ok());
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, beat);
    }

    #[test]
    fn disk_stats_round_trip_and_stay_optional() {
        let mut beat = heartbeat();
        let mut host = host();
        beat.host = Some(host.clone());
        let json = serde_json::to_string(&beat).unwrap();
        assert!(json.contains("\"disk_free_bytes\":1000000"), "{json}");
        assert!(json.contains("\"disk_total_bytes\":8000000"), "{json}");
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.host, Some(host.clone()));

        host.disk_free_bytes = None;
        host.disk_total_bytes = None;
        beat.host = Some(host);
        let json = serde_json::to_string(&beat).unwrap();
        assert!(!json.contains("disk_"), "{json}");
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert!(parsed.host.is_some());
        assert!(parsed.host.unwrap().disk_free_bytes.is_none());
    }

    #[test]
    fn heartbeats_without_stats_keep_the_old_shape() {
        let beat = heartbeat();
        let out = serde_json::to_string(&beat).unwrap();
        assert!(!out.contains("\"host\""), "{out}");
        assert!(!out.contains("\"jobs\""), "{out}");
        let parsed: Heartbeat =
            serde_json::from_str(r#"{"node":"peer-a","harness":"idle"}"#).unwrap();
        assert!(parsed.host.is_none());
        assert!(parsed.jobs.is_empty());
    }

    #[test]
    fn oversized_stats_drop_processes_then_jobs() {
        let mut beat = heartbeat();
        beat.host = Some(host());
        let mut big = job("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        big.processes = (0..300)
            .map(|index| ProcessStats {
                pid: index,
                ppid: 1,
                state: "S".to_owned(),
                cpu_secs: u64::from(index),
                rss_kb: 1,
                age_secs: 1,
                cmdline: "x".repeat(80),
                open_files: 0,
            })
            .collect();
        beat.jobs = vec![big];
        beat.trim_stats();
        assert!(!beat.jobs.is_empty());
        assert!(beat.jobs[0].processes.len() < 300);
        assert!(beat.stats_len() <= MAX_STATS_BYTES);

        // Even with no process entries left, whole jobs are dropped.
        beat.jobs = (0..200)
            .map(|_| {
                let mut job = job("01ARZ3NDEKTSV4RRFFQ69G5FAV");
                job.processes.clear();
                job
            })
            .collect();
        beat.trim_stats();
        assert!(beat.stats_len() <= MAX_STATS_BYTES);
        assert!(beat.jobs.len() < 200);
    }

    #[test]
    fn an_incoming_oversized_heartbeat_is_trimmed() {
        let mut beat = heartbeat();
        beat.host = Some(host());
        beat.jobs = vec![job("01ARZ3NDEKTSV4RRFFQ69G5FAV")];
        beat.jobs[0].processes = (0..300)
            .map(|index| ProcessStats {
                pid: index,
                ppid: 1,
                state: "S".to_owned(),
                cpu_secs: u64::from(index),
                rss_kb: 1,
                age_secs: 1,
                cmdline: "y".repeat(80),
                open_files: 0,
            })
            .collect();
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert!(parsed.jobs[0].processes.len() < 300);
        let stats = NodeStats {
            host: parsed.host,
            jobs: parsed.jobs,
            labels: Vec::new(),
            label_sources: BTreeMap::new(),
            workspace_bytes: 0,
            workspace_count: 0,
            workspace_reclaimed_bytes_total: 0,
            workspace_evictions_total: 0,
            workspace_delete_errors_total: 0,
            gc_removed_total: 0,
            gc_bytes_freed_total: 0,
            workspaces: Vec::new(),
        };
        assert!(serde_json::to_vec(&stats).unwrap().len() <= MAX_STATS_BYTES);
    }

    #[test]
    fn provider_states_round_trip_as_snake_case_strings() {
        for state in [
            ProviderState::Ok,
            ProviderState::Cooling,
            ProviderState::Disabled,
            ProviderState::Unknown,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<ProviderState>(&json).unwrap(), state);
        }
        for state in [HarnessState::Idle, HarnessState::Busy] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<HarnessState>(&json).unwrap(), state);
        }
    }

    #[test]
    fn provider_and_harness_states_round_trip_through_display_and_from_str() {
        for state in [
            ProviderState::Ok,
            ProviderState::Cooling,
            ProviderState::Disabled,
            ProviderState::Unknown,
        ] {
            let text = state.to_string();
            assert_eq!(ProviderState::from_str(&text).unwrap(), state, "{text}");
        }
        for state in [HarnessState::Idle, HarnessState::Busy] {
            let text = state.to_string();
            assert_eq!(HarnessState::from_str(&text).unwrap(), state, "{text}");
        }
        assert!("paused".parse::<HarnessState>().is_err());
        assert!("paused".parse::<ProviderState>().is_err());
    }

    #[test]
    fn a_heartbeat_from_a_newer_agent_with_extra_fields_is_accepted() {
        let json = serde_json::json!({
            "node": "n1",
            "harness": "idle",
            "providers": [{"name": "p", "state": "ok", "future_field": 1}],
            "host": {
                "load1": 0.1, "load5": 0.1, "load15": 0.1, "mem_total_kb": 1,
                "mem_available_kb": 1, "swap_used_kb": 0, "disk_free_bytes": 1,
                "disk_total_bytes": 2, "uptime_secs": 3, "future_host_field": true
            },
            "future_top_level": {"a": 1}
        });
        let beat: Heartbeat = serde_json::from_value(json).unwrap();
        assert_eq!(beat.providers.len(), 1);
        assert!(beat.validate().is_ok());
    }
}
