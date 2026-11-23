//! The controller's store. [`Store`] is the contract the HTTP layer is written against; the SQLite
//! implementation lives in `sqlite.rs`.
//!
//! Every method is synchronous and short; callers run it on a blocking thread. Time arrives as a
//! [`Now`] so tests control it: lease expiry uses `mono_ms` only (a clock that never jumps), the
//! `*_at` fields shown to people use `wall_unix`.

pub mod backup;
pub mod sqlite;

/// Schema version this binary creates and supports.
pub const SCHEMA_VERSION: i64 = 5;

#[cfg(test)]
pub mod conformance;
#[cfg(test)]
mod model;

use std::collections::HashMap;

use lu_proto::{
    ArtifactName, Completion, FailureClass, Heartbeat, JobEdit, JobId, JobSpec, JobState, NodeName,
    ProviderState, PublishRecord, StatsAggregate, StatsQuery, StatsReport,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub wall_unix: i64,
    pub mono_ms: u64,
}

/// Bounds checked atomically when a child is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeLimits {
    pub max_children: u64,
    pub max_descendants: u64,
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

/// Who a node's drain belongs to. Stored in the existing `nodes.draining` integer so no schema
/// change is needed: 0 is undrained, 1 is an operator drain and 2 is an agent's own drain.
/// Only an agent drain can be cleared by that agent's token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOrigin {
    None,
    Operator,
    Agent,
}

impl DrainOrigin {
    pub fn from_raw(raw: i64) -> Self {
        match raw {
            1 => Self::Operator,
            2 => Self::Agent,
            _ => Self::None,
        }
    }

    pub fn as_raw(self) -> i64 {
        match self {
            Self::None => 0,
            Self::Operator => 1,
            Self::Agent => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Operator => "operator",
            Self::Agent => "agent",
        }
    }

    pub fn is_draining(self) -> bool {
        self != Self::None
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
    /// Number of secret-like matches the controller scrubbed from this job's artifacts and logs.
    pub scrubbed_hits: u64,
    /// Whether the stored `diff.patch` was scrubbed; such a patch may no longer apply.
    pub patch_scrubbed: bool,
    /// High-confidence secret hits in added patch lines when patch scrubbing is off.
    pub patch_secret_hits: lu_proto::view::PatchSecretHitsView,
    /// Dependencies that are not yet `done`; empty once the job may run. A job blocked by a dead
    /// predecessor keeps the dead id here until it is retried or expires.
    pub waiting_on: Vec<JobId>,
    /// Commit an agent staged for a stacked-chain predecessor, recorded while the job is leased.
    pub staged_commit: Option<String>,
    /// Why a blocked job is waiting, currently `predecessor failed` after a dead dependency.
    pub blocked_reason: Option<String>,
    /// Wall-clock time when `blocked_reason` was set, used by the retry/TTL grace period.
    pub blocked_since: Option<i64>,
    /// Set only for jobs submitted through a harness's scoped socket.
    pub parent_job_id: Option<JobId>,
    /// The root of the spawn tree; equal to this job's id for a top-level job.
    pub root_job_id: Option<JobId>,
    /// Distance from the root job: top-level jobs have depth 0.
    pub depth: u32,
    /// Publish status, set only for jobs with a publish step after an attempt.
    pub publish: Option<PublishRecord>,
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
    /// Blocked jobs whose predecessor failed and which are waiting for a retry or TTL expiry.
    pub waiting_dependents: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeRecord {
    pub name: NodeName,
    pub last_seen: i64,
    /// Seen within two lease periods of the `now` given to the query.
    pub online: bool,
    pub draining: bool,
    /// Who requested the drain; `None` when the node is not drained.
    pub drain_origin: DrainOrigin,
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
    /// A configured scope limit was reached. The name is safe for the `limit_exceeded` error code
    /// and metrics label.
    LimitExceeded(&'static str),
    Internal(String),
}

/// Draft inventory used by the controller to enforce limits and show `lu top`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftCounts {
    /// Number of jobs in `draft`.
    pub total: u64,
    /// Number of drafts per `origin` label. Unlabelled drafts use the empty string.
    pub by_origin: Vec<(String, u64)>,
    /// Wall-clock time of the oldest draft, if any.
    pub oldest_created_at: Option<i64>,
}

pub trait Store: Send + Sync + 'static {
    /// Creates a job, or returns the existing one for the same key and spec (`false`).
    fn create_job(&self, spec: &JobSpec, now: Now) -> Result<(JobRecord, bool), StoreError>;
    /// Creates a draft job while enforcing draft capacity atomically. Evicts oldest drafts as
    /// needed (same origin for per-source overflow, global for total overflow). Returns
    /// `LimitExceeded("max_drafts")` when capacity cannot be freed.
    fn create_draft_job(
        &self,
        spec: &JobSpec,
        now: Now,
        max_total: u64,
        max_per_source: u64,
    ) -> Result<(JobRecord, bool), StoreError> {
        if !spec.draft {
            return self.create_job(spec, now);
        }
        let origin = spec.origin.clone().unwrap_or_default();
        let counts = self.draft_counts()?;
        let per_source = counts
            .by_origin
            .iter()
            .find(|(label, _)| label == &origin)
            .map(|(_, count)| *count)
            .unwrap_or(0);
        let per_source_overflow = per_source.saturating_add(1).saturating_sub(max_per_source);
        if per_source_overflow > 0 {
            self.expire_oldest_drafts(per_source_overflow, now, Some(&origin))?;
        }
        let counts = self.draft_counts()?;
        let total_overflow = counts.total.saturating_add(1).saturating_sub(max_total);
        if total_overflow > 0 {
            self.expire_oldest_drafts(total_overflow, now, None)?;
        }
        let counts = self.draft_counts()?;
        let per_source = counts
            .by_origin
            .iter()
            .find(|(label, _)| label == &origin)
            .map(|(_, count)| *count)
            .unwrap_or(0);
        if counts.total >= max_total || per_source >= max_per_source {
            return Err(StoreError::LimitExceeded("max_drafts"));
        }
        self.create_job(spec, now)
    }
    /// Creates a child job with explicit lineage. The store owns the lineage write so a child is
    /// never recorded without its parent and root. Stores that do not implement scope return an
    /// error rather than silently dropping the right.
    fn create_scoped_job(
        &self,
        spec: &JobSpec,
        now: Now,
        parent_job_id: &JobId,
        root_job_id: &JobId,
        depth: u32,
        limits: ScopeLimits,
    ) -> Result<(JobRecord, bool), StoreError> {
        let _ = (spec, now, parent_job_id, root_job_id, depth, limits);
        Err(StoreError::InvalidSpec(
            "job scoping is not supported by this store".to_owned(),
        ))
    }
    /// Children of one job, newest first.
    fn children(&self, parent_job_id: &JobId) -> Result<Vec<JobRecord>, StoreError> {
        let _ = parent_job_id;
        Ok(Vec::new())
    }
    /// Number of descendants with this root, excluding the root itself.
    fn descendants(&self, root_job_id: &JobId) -> Result<u64, StoreError> {
        let _ = root_job_id;
        Ok(0)
    }
    fn get_job(&self, id: &JobId) -> Result<JobRecord, StoreError>;
    /// Reads a job only while it is actively leased to `node` at `epoch`. The monotonic lease
    /// deadline and state are checked in one query so an expired lease cannot be used after the
    /// maintenance task has missed a tick. Stores without lease columns fall back to the stable
    /// fields in [`JobRecord`].
    fn leased_job(
        &self,
        id: &JobId,
        node: &NodeName,
        epoch: u64,
        now: Now,
        lease_ms: u64,
    ) -> Result<JobRecord, StoreError> {
        let _ = (now, lease_ms);
        let job = self.get_job(id)?;
        if job.node.as_ref() != Some(node)
            || job.epoch != epoch
            || !matches!(
                job.state,
                JobState::Leased | JobState::Running | JobState::NeedsInput
            )
        {
            return Err(StoreError::StaleEpoch);
        }
        Ok(job)
    }
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

    /// Records the 40-character lowercase hex commit an agent staged for a stacked chain. The job
    /// must be leased or running at `epoch`; a stale epoch or a finished job is rejected.
    fn record_staged_commit(
        &self,
        id: &JobId,
        epoch: u64,
        commit: &str,
        now: Now,
    ) -> Result<JobRecord, StoreError> {
        let _ = (id, epoch, commit, now);
        Err(StoreError::InvalidSpec(
            "staged commits are not supported by this store".to_owned(),
        ))
    }

    /// Direct dependents waiting on `dep` because that predecessor failed or was cancelled.
    fn blocked_dependents(&self, dep: &JobId) -> Result<Vec<JobRecord>, StoreError> {
        let _ = dep;
        Ok(Vec::new())
    }

    /// Fails blocked dependents whose retry/TTL grace period has elapsed. Returns the ids moved.
    fn expire_blocked_dependents(&self, now: Now, ttl_secs: u64) -> Result<Vec<JobId>, StoreError> {
        let _ = (now, ttl_secs);
        Ok(Vec::new())
    }

    /// Cancels a job; by default only the named job. Stores that track dependency waits can also
    /// cancel the blocked dependents that can no longer run.
    fn cancel_job_with_dependents(
        &self,
        id: &JobId,
        now: Now,
        keep_dependents: bool,
    ) -> Result<JobRecord, StoreError> {
        let _ = keep_dependents;
        self.cancel_job(id, now)
    }

    /// Creates a fresh terminal retry of `id` from `spec`. When `chain` is set, blocked dependents
    /// are cloned too, remapped onto the new ids. Returns each `(old, new)` pair created.
    fn retry_with_dependents(
        &self,
        id: &JobId,
        spec: &JobSpec,
        chain: bool,
        now: Now,
    ) -> Result<(JobRecord, Vec<(JobId, JobId)>), StoreError> {
        let _ = (id, spec, chain, now);
        Err(StoreError::InvalidSpec(
            "retrying with dependents is not supported by this store".to_owned(),
        ))
    }

    /// Changes the provider, model, fallbacks, labels, taints or dependencies of a queued or
    /// blocked job, keeping its id. Other states conflict. New dependencies must exist, be alive,
    /// number at most 16 and must not form a cycle. The job moves between `blocked` and `queued`
    /// as its dependencies change.
    fn edit_job(&self, id: &JobId, edit: &JobEdit, now: Now) -> Result<JobRecord, StoreError>;

    /// Drafts, oldest first.
    fn list_drafts(&self, limit: usize) -> Result<Vec<JobRecord>, StoreError> {
        let _ = limit;
        Ok(Vec::new())
    }
    /// Inventory of drafts for limits, `lu top` and metrics.
    fn draft_counts(&self) -> Result<DraftCounts, StoreError> {
        Ok(DraftCounts::default())
    }
    /// Moves a draft to `queued` or `blocked`. Only those two targets are accepted.
    fn approve_draft(
        &self,
        id: &JobId,
        target: JobState,
        now: Now,
    ) -> Result<JobRecord, StoreError> {
        let _ = (id, target, now);
        Err(StoreError::InvalidSpec(
            "drafts are not supported by this store".to_owned(),
        ))
    }
    /// Moves every draft created strictly before `older_than` to terminal `expired`.
    fn expire_drafts(&self, older_than: i64, now: Now) -> Result<u64, StoreError> {
        let _ = (older_than, now);
        Ok(0)
    }
    /// Expires up to `count` oldest drafts to make room under a capacity limit. Returns the
    /// number moved to terminal `expired`. Drafts are never deleted silently. When `origin` is
    /// set, only drafts with that origin label are considered.
    fn expire_oldest_drafts(
        &self,
        count: u64,
        now: Now,
        origin: Option<&str>,
    ) -> Result<u64, StoreError> {
        let _ = (count, now, origin);
        Ok(0)
    }

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
    /// Adds scrub counts for a stored artifact or log chunk. `patch` marks a scrubbed diff.
    fn record_scrub(
        &self,
        id: &JobId,
        epoch: u64,
        hits: u64,
        patch: bool,
    ) -> Result<(), StoreError>;
    /// Records high-confidence secret hits found in added patch lines.
    fn record_patch_secret_hits(
        &self,
        id: &JobId,
        epoch: u64,
        hits: &lu_proto::view::PatchSecretHitsView,
    ) -> Result<(), StoreError>;
    /// Records (or replaces) publish status for a job. No credential is ever stored here.
    fn record_publish(
        &self,
        id: &JobId,
        record: &PublishRecord,
        now: Now,
    ) -> Result<JobRecord, StoreError>;
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

    /// Drains or undrains a node as an operator. `Err(StoreError::NotFound)` for a node that has
    /// never heartbeated. An operator undrain clears a drain from either origin.
    fn set_draining(&self, node: &NodeName, draining: bool) -> Result<(), StoreError>;
    /// Drains a node on behalf of its own agent. Idempotent; an existing operator drain is left
    /// untouched so the agent cannot take ownership of it.
    fn set_agent_drain(&self, node: &NodeName) -> Result<(), StoreError>;
    /// Clears a drain the agent started. Returns whether the drain was actually removed. An
    /// operator drain yields `StoreError::Conflict` and stays in place.
    fn clear_agent_drain(&self, node: &NodeName) -> Result<bool, StoreError>;
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

    /// Aggregates terminal jobs into fleet and per-node contributions. The same report backs the
    /// operator API, the CLI and `/metrics`, so no view can invent a different number.
    ///
    /// There is no job-pruning path: `jobs` is the persistent source. Callers that need monotonic
    /// counters pass `since: None`, so a rolling window never moves a counter backwards.
    fn stats(&self, query: &StatsQuery) -> Result<StatsReport, StoreError> {
        Ok(StatsReport {
            by: query.by,
            since: query.since,
            node: query.node.clone(),
            fleet: StatsAggregate::empty(),
            nodes: Vec::new(),
        })
    }

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
