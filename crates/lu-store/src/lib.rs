//! The controller's store. [`Store`] is the contract the HTTP layer is written against; the SQLite
//! implementation lives in `sqlite.rs`.
//!
//! Every method is synchronous and short; callers run it on a blocking thread. Time arrives as a
//! [`Now`] so tests control it: lease expiry uses `mono_ms` only (a clock that never jumps), the
//! `*_at` fields shown to people use `wall_unix`.

pub mod backup;
pub mod sqlite;

/// Schema version this binary creates and supports.
pub const SCHEMA_VERSION: i64 = 4;

#[cfg(test)]
pub mod conformance;

use lu_proto::{
    ArtifactName, Completion, FailureClass, Heartbeat, JobId, JobSpec, JobState, NodeName,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub wall_unix: i64,
    pub mono_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub id: JobId,
    pub state: JobState,
    pub spec: JobSpec,
    pub attempt: u32,
    pub node: Option<NodeName>,
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
}

/// Every required label is present. Labels are compared exactly; Lu assigns no meaning to them.
pub fn labels_satisfied(required: &[String], node_labels: &[String]) -> bool {
    required
        .iter()
        .all(|label| node_labels.iter().any(|node| node == label))
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

/// Why a queued job cannot be placed, when no online node carries all of its labels.
pub fn waiting_reason(job: &JobRecord, nodes: &[NodeRecord]) -> Option<String> {
    if job.state != JobState::Queued {
        return None;
    }
    missing_labels(&job.spec.requires, nodes)
        .first()
        .map(|label| format!("no node with label {label}"))
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

    /// Records the heartbeat and the time it was seen.
    fn heartbeat(&self, hb: &Heartbeat, now: Now) -> Result<(), StoreError>;
    /// Leases the oldest queued job this node may run (not draining, harness usable, no job
    /// held, provider named by the job not cordoned and offered by the node), bumping the epoch.
    fn next_assignment(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
    ) -> Result<Option<Assignment>, StoreError>;
    /// Jobs held by `node` whose cancel was requested, with the epoch to name.
    fn pending_cancels(&self, node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError>;
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
    fn record_artifact(&self, id: &JobId, epoch: u64, name: ArtifactName)
        -> Result<(), StoreError>;
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

    /// Returns counts of jobs per state, including zero for absent states.
    /// Also returns total nodes, online nodes, drained nodes, and completed job counters.
    fn job_counts(&self, now: Now, lease_ms: u64) -> Result<JobCounts, StoreError>;
}
