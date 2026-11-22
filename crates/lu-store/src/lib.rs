//! The controller's store. [`Store`] is the contract the HTTP layer is written against; the SQLite
//! implementation lives in `sqlite.rs`.
//!
//! Every method is synchronous and short; callers run it on a blocking thread. Time arrives as a
//! [`Now`] so tests control it: lease expiry uses `mono_ms` only (a clock that never jumps), the
//! `*_at` fields shown to people use `wall_unix`.

pub mod backup;
pub mod sqlite;

/// Schema version this binary creates and supports.
pub const SCHEMA_VERSION: i64 = 1;

#[cfg(test)]
pub mod conformance;
#[cfg(test)]
mod model;

use std::collections::HashMap;

use lu_proto::{
    ArtifactName, Completion, FailureClass, Heartbeat, JobEdit, JobId, JobSpec, JobState, NodeName,
    ProviderState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub wall_unix: i64,
    pub mono_ms: u64,
}

/// Where a provider cordon came from. Reiki-derived cordons lift after healthy heartbeats;
/// failure-derived cordons run for their time limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CordonSource {
    Reiki,
    Failure,
}

impl CordonSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CordonSource::Reiki => "reiki",
            CordonSource::Failure => "failure",
        }
    }
}

/// A provider cordon with the detail needed to explain it to an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cordon {
    pub node: NodeName,
    pub provider: String,
    pub source: CordonSource,
    pub reason: String,
    pub set_at_unix: i64,
    /// Monotonic deadline used by dispatch; `None` only for migrated rows without a usable clock.
    pub until_mono_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub id: JobId,
    pub state: JobState,
    pub spec: JobSpec,
    pub attempt: u32,
    pub node: Option<NodeName>,
    /// The provider choice dispatch made for the current attempt, if any.
    pub assigned_provider: Option<String>,
    /// The model that goes with `assigned_provider`, if any.
    pub assigned_model: Option<String>,
    pub epoch: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub failure_class: Option<FailureClass>,
    pub summary: Option<String>,
    pub exit_code: Option<i32>,
    pub artifacts: Vec<ArtifactName>,
    pub cancel_requested: bool,
    pub log_len: u64,
    /// Size of the stored `diff.patch` in bytes when recorded. Jobs from before schema 6, or
    /// without a diff, have no size.
    pub diff_bytes: Option<u64>,
    /// A `done` job whose expectations were not met in suspect mode.
    pub expect_failed: bool,
    /// Dependencies that are not yet `done`; empty once the job may run.
    pub waiting_on: Vec<JobId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCounts {
    pub jobs: Vec<(JobState, u64)>,
    pub nodes: u64,
    pub nodes_online: u64,
    pub nodes_drained: u64,
    pub completed_done: u64,
    pub completed_failed: u64,
    pub completed_cancelled: u64,
    /// Creation time of the oldest `queued` or `blocked` job, if any.
    pub oldest_pending_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeRecord {
    pub name: NodeName,
    pub last_seen: i64,
    /// Seen within two lease periods of the `now` given to the query.
    pub online: bool,
    pub draining: bool,
    pub heartbeat: Heartbeat,
    pub current_job: Option<JobId>,
    /// Providers with an unexpired `(node, provider)` cordon.
    pub cordons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub job_id: JobId,
    pub epoch: u64,
    pub spec: JobSpec,
    /// The chosen provider choice; `None` for a job that names no provider.
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Every required label is present. Labels are compared exactly; Lu assigns no meaning to them.
pub fn labels_satisfied(required: &[String], node_labels: &[String]) -> bool {
    required
        .iter()
        .all(|label| node_labels.iter().any(|node| node == label))
}

/// Every taint on the node is tolerated by the job. An untainted node always passes.
pub fn taints_tolerated(node_taints: &[String], tolerates: &[String]) -> bool {
    node_taints.iter().all(|taint| tolerates.contains(taint))
}

/// The provider/model pairs a node can serve for a `provider = "any"` job, in the preference order
/// the agent published. A pair is usable only while its provider is healthy in the latest
/// heartbeat and the node is not cordoned for that provider.
pub fn healthy_provider_models(node: &NodeRecord) -> Vec<&lu_proto::ProviderModel> {
    node.heartbeat
        .provider_models
        .iter()
        .filter(|pair| {
            !node.cordons.contains(&pair.provider)
                && node
                    .heartbeat
                    .providers
                    .iter()
                    .any(|status| status.name == pair.provider && status.state == ProviderState::Ok)
        })
        .collect()
}

/// The required labels no online node advertises, in the job's order.
pub fn missing_labels<'a>(required: &'a [String], nodes: &[NodeRecord]) -> Vec<&'a str> {
    let online = || {
        nodes
            .iter()
            .filter(|node| node.online)
            .flat_map(|node| node.heartbeat.labels.iter())
    };
    required
        .iter()
        .filter(|label| !online().any(|node| node == *label))
        .map(String::as_str)
        .collect()
}

/// Why a queued job cannot be placed, including a configured group that is at its cap.
pub fn waiting_reason_with_groups(
    job: &JobRecord,
    nodes: &[NodeRecord],
    group_counts: &HashMap<String, usize>,
    group_limits: &HashMap<String, usize>,
) -> Option<String> {
    if job.state != JobState::Queued {
        return None;
    }
    if let Some(group) = &job.spec.group {
        if let Some(&limit) = group_limits.get(group) {
            let running = group_counts.get(group).copied().unwrap_or(0);
            if running >= limit {
                return Some(format!("group limit ({group} {running}/{limit})"));
            }
        }
    }
    waiting_reason(job, nodes)
}

/// Why a queued job cannot be placed: first a missing label, then a taint no matching node is
/// tolerated for.
pub fn waiting_reason(job: &JobRecord, nodes: &[NodeRecord]) -> Option<String> {
    if job.state != JobState::Queued {
        return None;
    }
    if let Some(label) = missing_labels(&job.spec.requires, nodes).first() {
        return Some(format!("no node with label {label}"));
    }
    // Only online nodes that already satisfy the label rules matter here: the taint reason is a
    // more specific explanation than the label one and must not replace it.
    let eligible: Vec<&NodeRecord> = nodes
        .iter()
        .filter(|node| node.online && labels_satisfied(&job.spec.requires, &node.heartbeat.labels))
        .collect();
    if !eligible.is_empty()
        && eligible
            .iter()
            .all(|node| !taints_tolerated(&node.heartbeat.taints, &job.spec.tolerates))
    {
        let taint = eligible
            .iter()
            .flat_map(|node| node.heartbeat.taints.iter())
            .find(|taint| !job.spec.tolerates.contains(taint))?;
        return Some(format!("only tainted nodes are free: {taint}"));
    }
    if job.spec.is_any_provider()
        && !eligible.iter().any(|node| {
            taints_tolerated(&node.heartbeat.taints, &job.spec.tolerates)
                && !healthy_provider_models(node).is_empty()
        })
    {
        return Some("no node has a healthy provider for provider \"any\"".to_owned());
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    /// Same idempotency key, different spec; or an illegal state edge; or a log offset that is not
    /// the stored length (`length` carries it).
    Conflict {
        length: Option<u64>,
    },
    /// The write names an epoch older than the job's.
    StaleEpoch,
    /// `after` names a job that does not exist.
    InvalidDependency(JobId),
    /// `after` names a job that already failed or was cancelled, so the new job could never run.
    DeadDependency(JobId),
    /// Editing `after` would make the job depend on itself through its dependents.
    DependencyCycle,
    /// The spec itself is invalid; normally the API validates before reaching the store.
    InvalidSpec(String),
    Internal(String),
}

pub trait Store: Send + Sync + 'static {
    /// Creates a job, or returns the existing one for the same key and spec (`false`).
    fn create_job(&self, spec: &JobSpec, now: Now) -> Result<(JobRecord, bool), StoreError>;
    fn get_job(&self, id: &JobId) -> Result<JobRecord, StoreError>;
    /// The newest job submitted with an idempotency key, if there is one.
    fn job_by_key(&self, key: &str) -> Option<JobRecord>;
    /// Newest first. `after` is a cursor: jobs strictly older than it.
    fn list_jobs(
        &self,
        state: Option<JobState>,
        limit: usize,
        after: Option<&JobId>,
    ) -> Result<Vec<JobRecord>, StoreError>;
    /// Queued or blocked becomes cancelled; leased or running sets `cancel_requested`;
    /// terminal conflicts.
    fn cancel_job(&self, id: &JobId, now: Now) -> Result<JobRecord, StoreError>;

    /// Changes the provider, model, fallbacks, labels, taints or dependencies of a queued or
    /// blocked job, keeping its id. Other states conflict. New dependencies must exist, be alive,
    /// number at most 16 and must not form a cycle. The job moves between `blocked` and `queued`
    /// as its dependencies change.
    fn edit_job(&self, id: &JobId, edit: &JobEdit, now: Now) -> Result<JobRecord, StoreError>;

    /// Records the heartbeat and the time it was seen.
    fn heartbeat(&self, hb: &Heartbeat, now: Now) -> Result<(), StoreError>;
    /// Leases the highest-priority queued job this node may run, earliest submission first within a priority
    /// (not draining, harness usable, no job held, labels satisfied, taints tolerated, able to
    /// serve the primary or one fallback provider choice, and below its group's cap), bumping the
    /// epoch. Provider, label and taint rules still apply first.
    fn next_assignment(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
    ) -> Result<Option<Assignment>, StoreError>;
    /// Jobs held by `node` whose cancel was requested, with the epoch to name.
    fn pending_cancels(&self, node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError>;
    /// Jobs currently held by `node` (leased, running or recovering), with the epoch to name.
    fn held_jobs(&self, node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError>;
    /// Leased to running.
    fn start_job(&self, id: &JobId, epoch: u64, now: Now) -> Result<JobRecord, StoreError>;
    /// Extends the lease; returns whether a cancel is pending.
    fn renew_lease(
        &self,
        id: &JobId,
        epoch: u64,
        now: Now,
        lease_ms: u64,
    ) -> Result<bool, StoreError>;
    /// Reserves `n` log bytes at `offset`; conflicts with the stored length when they differ.
    /// Returns the new length. The caller then writes the bytes to the log file.
    fn reserve_log(&self, id: &JobId, epoch: u64, offset: u64, n: u64) -> Result<u64, StoreError>;
    /// Releases a reservation that was not written. `length` is the reservation's returned
    /// length and `n` the bytes it reserved; conflicts when either no longer matches.
    fn unreserve_log(&self, id: &JobId, epoch: u64, length: u64, n: u64)
        -> Result<u64, StoreError>;
    /// Records an artifact name and its byte size, used for diff-based outcome notes.
    fn record_artifact(
        &self,
        id: &JobId,
        epoch: u64,
        name: ArtifactName,
        bytes: u64,
    ) -> Result<(), StoreError>;
    /// Finishes the attempt. Success is `done`; a node or provider failure with attempts left
    /// returns the job to `queued` (attempt + 1) and cordons `(node, provider)` for a provider
    /// failure; anything else is `failed`. `cordon_ms` and `max_retries` come from config.
    fn complete(
        &self,
        completion: &Completion,
        failure: Option<(FailureClass, Option<String>)>,
        now: Now,
        max_retries: u32,
        cordon_ms: u64,
    ) -> Result<JobRecord, StoreError>;

    /// Leases not renewed within their window become `recovering`. Returns the jobs moved.
    fn expire_leases(&self, now: Now) -> Result<Vec<JobId>, StoreError>;
    /// `recovering` for longer than `grace_ms` returns to `queued` (class `node`, attempt + 1) or
    /// fails when attempts are exhausted. Returns the jobs moved.
    fn requeue_recovering(
        &self,
        now: Now,
        grace_ms: u64,
        max_retries: u32,
    ) -> Result<Vec<JobId>, StoreError>;
    /// At controller start: every `leased` or `running` job becomes `recovering`.
    fn recover_on_boot(&self, now: Now) -> Result<u64, StoreError>;

    /// Drains or undrains a node. `Err(StoreError::NotFound)` for a node that has never
    /// heartbeated.
    fn set_draining(&self, node: &NodeName, draining: bool) -> Result<(), StoreError>;
    fn nodes(&self, now: Now, lease_ms: u64) -> Result<Vec<NodeRecord>, StoreError>;

    /// All cordons, or just one node's when `node` is `Some`.
    fn cordons(&self, node: Option<&NodeName>) -> Result<Vec<Cordon>, StoreError>;
    /// Creates or replaces a provider cordon. Reiki cordons lift after two healthy heartbeats;
    /// failure cordons rely on `until_mono_ms`.
    fn set_cordon(
        &self,
        node: &NodeName,
        provider: &str,
        source: CordonSource,
        reason: &str,
        set_at_unix: i64,
        until_mono_ms: i64,
    ) -> Result<(), StoreError>;
    /// Removes cordons for a node, optionally only one provider. Returns the providers removed.
    fn uncordon(&self, node: &NodeName, provider: Option<&str>) -> Result<Vec<String>, StoreError>;

    /// Returns counts of jobs per state, including zero for absent states.
    /// Also returns total nodes, online nodes, drained nodes, and completed job counters.
    fn job_counts(&self, now: Now, lease_ms: u64) -> Result<JobCounts, StoreError>;

    /// Installs the configured concurrency caps by group name. Unconfigured groups stay unlimited.
    /// Defaults to no caps for stores that do not manage dispatch.
    fn set_group_limits(&self, limits: HashMap<String, usize>) -> Result<(), StoreError> {
        let _ = limits;
        Ok(())
    }

    /// Counts jobs in `leased`, `running`, `recovering` or `needs_input` per group.
    fn group_counts(&self) -> Result<HashMap<String, usize>, StoreError> {
        Ok(HashMap::new())
    }
}
