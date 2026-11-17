//! In-memory [`crate::Store`] for tests.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use lu_proto::{
    ArtifactName, CancelEffect, Completion, FailureClass, Heartbeat, JobId, JobSpec, JobState,
    NodeName, Outcome, MAX_LOG_BYTES,
};

use crate::{Assignment, JobCounts, JobRecord, NodeRecord, Now, Store, StoreError};

#[derive(Debug, Clone)]
struct JobEntry {
    rec: JobRecord,
    lease_until_mono_ms: Option<u64>,
    recovering_since_mono_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct NodeEntry {
    heartbeat: Option<Heartbeat>,
    last_seen_wall: i64,
    last_seen_mono_ms: u64,
    draining: bool,
}

#[derive(Debug, Default)]
struct Inner {
    jobs: BTreeMap<String, JobEntry>, // key = JobId string for ordering
    nodes: HashMap<String, NodeEntry>,
    /// (node, provider) -> until_mono_ms
    cordons: HashMap<(String, String), u64>,
    /// idempotency_key -> (spec_hash, job_id)
    idempotency: HashMap<String, (String, String)>,
}

/// Simple in-memory store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn check_transition(from: JobState, to: JobState) -> Result<(), StoreError> {
    if JobState::can_transition(from, to) {
        Ok(())
    } else {
        Err(StoreError::Conflict { length: None })
    }
}

fn artifact_push(rec: &mut JobRecord, name: ArtifactName) {
    if !rec.artifacts.contains(&name) {
        rec.artifacts.push(name);
    }
}

fn job_held_by(inner: &Inner, node: &str) -> Option<String> {
    for (id, e) in &inner.jobs {
        if matches!(e.rec.state, JobState::Leased | JobState::Running) {
            if let Some(n) = &e.rec.node {
                if n.as_str() == node {
                    return Some(id.clone());
                }
            }
        }
    }
    None
}

fn provider_cordoned(inner: &Inner, node: &str, provider: &str, now: Now) -> bool {
    inner
        .cordons
        .get(&(node.to_owned(), provider.to_owned()))
        .map(|until| *until > now.mono_ms)
        .is_some_and(|x| x)
}

fn node_offers_provider(hb: &Heartbeat, provider: &str) -> bool {
    hb.providers
        .iter()
        .any(|p| p.name == provider && matches!(p.state, lu_proto::ProviderState::Ok))
}

fn waiting_for(inner: &Inner, after: &[JobId]) -> Vec<JobId> {
    after
        .iter()
        .filter(|dep| {
            inner
                .jobs
                .get(dep.as_str())
                .is_none_or(|e| e.rec.state != JobState::Done)
        })
        .cloned()
        .collect()
}

fn refresh_waiting(inner: &Inner, rec: &mut JobRecord) {
    rec.waiting_on = waiting_for(inner, &rec.spec.after);
}

fn unblock_dependents(inner: &mut Inner, dep: &str, now: Now) {
    let ids: Vec<String> = inner
        .jobs
        .iter()
        .filter(|(_, e)| {
            e.rec.state == JobState::Blocked
                && e.rec.spec.after.iter().any(|d| d.as_str() == dep)
                && e.rec.spec.after.iter().all(|d| {
                    inner
                        .jobs
                        .get(d.as_str())
                        .is_some_and(|j| j.rec.state == JobState::Done)
                })
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        let e = inner.jobs.get_mut(&id).unwrap();
        e.rec.state = JobState::Queued;
        e.rec.updated_at = now.wall_unix;
        e.rec.failure_class = None;
        e.rec.summary = None;
        e.rec.finished_at = None;
    }
}

fn fail_dependents(inner: &mut Inner, dep: &str, dep_state: &str, now: Now) {
    let mut queue = vec![(dep.to_owned(), dep_state.to_owned())];
    while let Some((dep_id, dep_state)) = queue.pop() {
        let ids: Vec<String> = inner
            .jobs
            .iter()
            .filter(|(_, e)| {
                e.rec.state == JobState::Blocked
                    && e.rec.spec.after.iter().any(|d| d.as_str() == dep_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let e = inner.jobs.get_mut(&id).unwrap();
            e.rec.state = JobState::Failed;
            e.rec.failure_class = Some(FailureClass::Dependency);
            e.rec.summary = Some(format!("dependency {dep_id} {dep_state}"));
            e.rec.finished_at = Some(now.wall_unix);
            e.rec.updated_at = now.wall_unix;
            e.rec.node = None;
            e.lease_until_mono_ms = None;
            e.recovering_since_mono_ms = None;
            queue.push((id, "failed".to_owned()));
        }
    }
}

impl Store for MemoryStore {
    fn create_job(&self, spec: &JobSpec, now: Now) -> Result<(JobRecord, bool), StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        spec.validate()
            .map_err(|e| StoreError::InvalidSpec(e.to_string()))?;
        let canonical = spec.canonical_json();
        if let Some(key) = &spec.idempotency_key {
            if let Some((hash, job_id)) = inner.idempotency.get(key).cloned() {
                let entry = inner
                    .jobs
                    .get(&job_id)
                    .ok_or_else(|| StoreError::Internal("dangling idempotency".into()))?;
                if hash == canonical {
                    return Ok((entry.rec.clone(), false));
                } else {
                    return Err(StoreError::Conflict { length: None });
                }
            }
        }
        let id = JobId::new();
        let mut blocked = false;
        for dep in &spec.after {
            let entry = inner
                .jobs
                .get(dep.as_str())
                .ok_or_else(|| StoreError::InvalidDependency(dep.clone()))?;
            if matches!(entry.rec.state, JobState::Failed | JobState::Cancelled) {
                return Err(StoreError::DeadDependency(dep.clone()));
            }
            if entry.rec.state != JobState::Done {
                blocked = true;
            }
        }
        let mut rec = JobRecord {
            id: id.clone(),
            state: if blocked {
                JobState::Blocked
            } else {
                JobState::Queued
            },
            spec: spec.clone(),
            attempt: 1,
            node: None,
            epoch: 0,
            created_at: now.wall_unix,
            updated_at: now.wall_unix,
            started_at: None,
            finished_at: None,
            failure_class: None,
            summary: None,
            exit_code: None,
            artifacts: Vec::new(),
            cancel_requested: false,
            log_len: 0,
            waiting_on: Vec::new(),
        };
        refresh_waiting(&inner, &mut rec);
        let entry = JobEntry {
            rec: rec.clone(),
            lease_until_mono_ms: None,
            recovering_since_mono_ms: None,
        };
        let id_s = id.as_str().to_owned();
        if let Some(key) = &spec.idempotency_key {
            inner
                .idempotency
                .insert(key.clone(), (canonical, id_s.clone()));
        }
        inner.jobs.insert(id_s, entry);
        Ok((rec, true))
    }

    fn get_job(&self, id: &JobId) -> Result<JobRecord, StoreError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        inner
            .jobs
            .get(id.as_str())
            .map(|e| {
                let mut rec = e.rec.clone();
                refresh_waiting(&inner, &mut rec);
                rec
            })
            .ok_or(StoreError::NotFound)
    }

    fn list_jobs(
        &self,
        state: Option<JobState>,
        limit: usize,
        after: Option<&JobId>,
    ) -> Result<Vec<JobRecord>, StoreError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let mut out: Vec<JobRecord> = inner
            .jobs
            .values()
            .filter(|e| state.map(|s| e.rec.state == s).unwrap_or(true))
            .map(|e| e.rec.clone())
            .collect();
        for rec in &mut out {
            refresh_waiting(&inner, rec);
        }
        // newest first => descending by id
        out.sort_by(|a, b| b.id.cmp(&a.id));
        if let Some(after) = after {
            out.retain(|r| r.id.as_str() < after.as_str());
        }
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    fn cancel_job(&self, id: &JobId, now: Now) -> Result<JobRecord, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let terminal = {
            let entry = inner
                .jobs
                .get_mut(id.as_str())
                .ok_or(StoreError::NotFound)?;
            match entry.rec.state.cancel_effect() {
                CancelEffect::Now => {
                    check_transition(entry.rec.state, JobState::Cancelled)?;
                    entry.rec.state = JobState::Cancelled;
                    entry.rec.updated_at = now.wall_unix;
                    entry.rec.finished_at = Some(now.wall_unix);
                    entry.lease_until_mono_ms = None;
                    entry.recovering_since_mono_ms = None;
                    true
                }
                CancelEffect::Request => {
                    entry.rec.cancel_requested = true;
                    entry.rec.updated_at = now.wall_unix;
                    false
                }
                CancelEffect::Refuse => return Err(StoreError::Conflict { length: None }),
            }
        };
        if terminal {
            fail_dependents(&mut inner, id.as_str(), "cancelled", now);
        }
        let mut rec = inner.jobs.get(id.as_str()).unwrap().rec.clone();
        refresh_waiting(&inner, &mut rec);
        Ok(rec)
    }

    fn heartbeat(&self, hb: &Heartbeat, now: Now) -> Result<(), StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let e = inner.nodes.entry(hb.node.as_str().to_owned()).or_default();
        e.heartbeat = Some(hb.clone());
        e.last_seen_wall = now.wall_unix;
        e.last_seen_mono_ms = now.mono_ms;
        Ok(())
    }

    fn next_assignment(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
    ) -> Result<Option<Assignment>, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        // node must exist in our view? heartbeat required? but draining/harness state come from node entry
        let (draining, usable, hb_clone) = {
            let ne = inner.nodes.get(node.as_str());
            let draining = ne.map(|n| n.draining).unwrap_or(false);
            let hb = ne.and_then(|n| n.heartbeat.clone());
            let usable = hb
                .as_ref()
                .map(|h| matches!(h.harness, lu_proto::HarnessState::Idle))
                .unwrap_or(false);
            (draining, usable, hb)
        };
        if draining || !usable {
            return Ok(None);
        }
        if job_held_by(&inner, node.as_str()).is_some() {
            return Ok(None);
        }
        let hb = match hb_clone {
            Some(h) => h,
            None => return Ok(None),
        };
        // oldest queued job => ascending by id
        let mut candidates: Vec<String> = inner
            .jobs
            .iter()
            .filter(|(_, e)| e.rec.state == JobState::Queued)
            .map(|(id, _)| id.clone())
            .collect();
        candidates.sort();
        for jid in candidates {
            let provider_req = {
                let e = inner.jobs.get(&jid).unwrap();
                e.rec.spec.provider.clone()
            };
            if let Some(p) = &provider_req {
                if provider_cordoned(&inner, node.as_str(), p, now) {
                    continue;
                }
                if !node_offers_provider(&hb, p) {
                    continue;
                }
            }
            let e = inner.jobs.get_mut(&jid).unwrap();
            check_transition(e.rec.state, JobState::Leased)?;
            e.rec.state = JobState::Leased;
            e.rec.node = Some(node.clone());
            e.rec.epoch = e.rec.epoch.saturating_add(1);
            e.rec.updated_at = now.wall_unix;
            e.rec.failure_class = None;
            e.rec.summary = None;
            e.rec.exit_code = None;
            e.rec.finished_at = None;
            e.lease_until_mono_ms = Some(now.mono_ms.saturating_add(lease_ms));
            e.recovering_since_mono_ms = None;
            return Ok(Some(Assignment {
                job_id: e.rec.id.clone(),
                epoch: e.rec.epoch,
                spec: e.rec.spec.clone(),
            }));
        }
        Ok(None)
    }

    fn pending_cancels(&self, node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let mut out = Vec::new();
        for e in inner.jobs.values() {
            if e.rec.cancel_requested {
                if let Some(n) = &e.rec.node {
                    if n == node && matches!(e.rec.state, JobState::Leased | JobState::Running) {
                        out.push((e.rec.id.clone(), e.rec.epoch));
                    }
                }
            }
        }
        Ok(out)
    }

    fn start_job(&self, id: &JobId, epoch: u64, now: Now) -> Result<JobRecord, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let e = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if e.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        check_transition(e.rec.state, JobState::Running)?;
        e.rec.state = JobState::Running;
        e.rec.updated_at = now.wall_unix;
        if e.rec.started_at.is_none() {
            e.rec.started_at = Some(now.wall_unix);
        }
        let mut rec = e.rec.clone();
        refresh_waiting(&inner, &mut rec);
        Ok(rec)
    }

    fn renew_lease(
        &self,
        id: &JobId,
        epoch: u64,
        now: Now,
        lease_ms: u64,
    ) -> Result<bool, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let e = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if e.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(
            e.rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        if e.rec.state == JobState::Recovering {
            check_transition(e.rec.state, JobState::Running)?;
            e.rec.state = JobState::Running;
            e.recovering_since_mono_ms = None;
        }
        e.lease_until_mono_ms = Some(now.mono_ms.saturating_add(lease_ms));
        e.rec.updated_at = now.wall_unix;
        Ok(e.rec.cancel_requested)
    }

    fn reserve_log(&self, id: &JobId, epoch: u64, offset: u64, n: u64) -> Result<u64, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let e = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if e.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(
            e.rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        if offset != e.rec.log_len {
            return Err(StoreError::Conflict {
                length: Some(e.rec.log_len),
            });
        }
        let cap = MAX_LOG_BYTES;
        let new_len = if e.rec.log_len >= cap {
            cap
        } else {
            let want = e.rec.log_len.saturating_add(n);
            if want > cap {
                cap
            } else {
                want
            }
        };
        e.rec.log_len = new_len;
        Ok(e.rec.log_len)
    }

    fn record_artifact(
        &self,
        id: &JobId,
        epoch: u64,
        name: ArtifactName,
    ) -> Result<(), StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let e = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if e.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(
            e.rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            // allow some flexibility? but likely only active attempts
        }
        artifact_push(&mut e.rec, name);
        Ok(())
    }

    fn complete(
        &self,
        completion: &Completion,
        failure: Option<(FailureClass, Option<String>)>,
        now: Now,
        max_retries: u32,
        cordon_ms: u64,
    ) -> Result<JobRecord, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        // take needed data
        let (node_for_cordon, current_epoch, current_state, attempt) = {
            let e = inner
                .jobs
                .get(completion.job_id.as_str())
                .ok_or(StoreError::NotFound)?;
            (e.rec.node.clone(), e.rec.epoch, e.rec.state, e.rec.attempt)
        };
        if completion.epoch != current_epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(
            current_state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        let e = inner.jobs.get_mut(completion.job_id.as_str()).unwrap();
        e.rec.exit_code = completion.exit_code;
        e.rec.summary = completion.summary.clone();
        match completion.outcome {
            Outcome::Done => {
                check_transition(e.rec.state, JobState::Done)?;
                e.rec.state = JobState::Done;
                e.rec.finished_at = Some(now.wall_unix);
                e.rec.updated_at = now.wall_unix;
                e.rec.failure_class = None;
                e.lease_until_mono_ms = None;
                e.recovering_since_mono_ms = None;
            }
            Outcome::Cancelled => {
                check_transition(e.rec.state, JobState::Cancelled)?;
                e.rec.state = JobState::Cancelled;
                e.rec.finished_at = Some(now.wall_unix);
                e.rec.updated_at = now.wall_unix;
                e.lease_until_mono_ms = None;
                e.recovering_since_mono_ms = None;
            }
            Outcome::Failed(_) => {
                let (class, provider) = failure.unwrap_or((FailureClass::Execution, None));
                e.rec.failure_class = Some(class);
                if class.retryable(attempt, max_retries) {
                    // retry: back to queued, attempt+1
                    check_transition(e.rec.state, JobState::Queued)?;
                    e.rec.state = JobState::Queued;
                    e.rec.attempt = attempt.saturating_add(1);
                    e.rec.node = None;
                    e.rec.cancel_requested = false;
                    e.rec.finished_at = None;
                    // keep started_at? probably keep history; not specified
                    e.lease_until_mono_ms = None;
                    e.recovering_since_mono_ms = None;
                    e.rec.updated_at = now.wall_unix;
                    if class == FailureClass::Provider {
                        if let (Some(node), Some(p)) = (node_for_cordon, provider) {
                            inner.cordons.insert(
                                (node.as_str().to_owned(), p),
                                now.mono_ms.saturating_add(cordon_ms),
                            );
                        }
                    }
                } else {
                    check_transition(e.rec.state, JobState::Failed)?;
                    e.rec.state = JobState::Failed;
                    e.rec.finished_at = Some(now.wall_unix);
                    e.rec.updated_at = now.wall_unix;
                    e.lease_until_mono_ms = None;
                    e.recovering_since_mono_ms = None;
                }
            }
        }
        let final_state = inner
            .jobs
            .get(completion.job_id.as_str())
            .unwrap()
            .rec
            .state;
        if final_state == JobState::Done {
            unblock_dependents(&mut inner, completion.job_id.as_str(), now);
        } else if matches!(final_state, JobState::Failed | JobState::Cancelled) {
            fail_dependents(
                &mut inner,
                completion.job_id.as_str(),
                if final_state == JobState::Cancelled {
                    "cancelled"
                } else {
                    "failed"
                },
                now,
            );
        }
        let mut rec = inner
            .jobs
            .get(completion.job_id.as_str())
            .unwrap()
            .rec
            .clone();
        refresh_waiting(&inner, &mut rec);
        Ok(rec)
    }

    fn expire_leases(&self, now: Now) -> Result<Vec<JobId>, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let mut moved = Vec::new();
        let ids: Vec<String> = inner.jobs.keys().cloned().collect();
        for id in ids {
            let e = inner.jobs.get_mut(&id).unwrap();
            if matches!(e.rec.state, JobState::Leased | JobState::Running) {
                if let Some(until) = e.lease_until_mono_ms {
                    if until <= now.mono_ms {
                        check_transition(e.rec.state, JobState::Recovering)?;
                        e.rec.state = JobState::Recovering;
                        e.rec.updated_at = now.wall_unix;
                        e.recovering_since_mono_ms = Some(now.mono_ms);
                        moved.push(e.rec.id.clone());
                    }
                }
            }
        }
        Ok(moved)
    }

    fn requeue_recovering(
        &self,
        now: Now,
        grace_ms: u64,
        max_retries: u32,
    ) -> Result<Vec<JobId>, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let mut moved = Vec::new();
        let mut failed = Vec::new();
        let ids: Vec<String> = inner.jobs.keys().cloned().collect();
        for id in ids {
            let e = inner.jobs.get_mut(&id).unwrap();
            if e.rec.state == JobState::Recovering {
                let since = e.recovering_since_mono_ms.unwrap_or(now.mono_ms);
                if now.mono_ms.saturating_sub(since) >= grace_ms {
                    if FailureClass::Node.retryable(e.rec.attempt, max_retries) {
                        check_transition(e.rec.state, JobState::Queued)?;
                        e.rec.state = JobState::Queued;
                        e.rec.attempt = e.rec.attempt.saturating_add(1);
                        e.rec.node = None;
                        e.rec.failure_class = None;
                        e.rec.cancel_requested = false;
                        e.rec.updated_at = now.wall_unix;
                        e.rec.finished_at = None;
                        e.lease_until_mono_ms = None;
                        e.recovering_since_mono_ms = None;
                    } else {
                        check_transition(e.rec.state, JobState::Failed)?;
                        e.rec.state = JobState::Failed;
                        e.rec.failure_class = Some(FailureClass::Node);
                        e.rec.updated_at = now.wall_unix;
                        e.rec.finished_at = Some(now.wall_unix);
                        e.lease_until_mono_ms = None;
                        e.recovering_since_mono_ms = None;
                        failed.push(e.rec.id.clone());
                    }
                    moved.push(e.rec.id.clone());
                }
            }
        }
        for id in failed {
            fail_dependents(&mut inner, id.as_str(), "failed", now);
        }
        Ok(moved)
    }

    fn recover_on_boot(&self, now: Now) -> Result<u64, StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let mut count = 0u64;
        for e in inner.jobs.values_mut() {
            if matches!(e.rec.state, JobState::Leased | JobState::Running) {
                check_transition(e.rec.state, JobState::Recovering)?;
                e.rec.state = JobState::Recovering;
                e.rec.updated_at = now.wall_unix;
                e.recovering_since_mono_ms = Some(now.mono_ms);
                count += 1;
            }
        }
        Ok(count)
    }

    fn set_draining(&self, node: &NodeName, draining: bool) -> Result<(), StoreError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        // Only a node that has heartbeated is known; draining a name that has not appeared yet
        // would persist nothing and be lost when the node first shows up.
        match inner.nodes.get_mut(node.as_str()) {
            Some(e) if e.heartbeat.is_some() => {
                e.draining = draining;
                Ok(())
            }
            _ => Err(StoreError::NotFound),
        }
    }

    fn nodes(&self, now: Now, lease_ms: u64) -> Result<Vec<NodeRecord>, StoreError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let mut out = Vec::new();
        for (name, ne) in &inner.nodes {
            let Some(hb) = &ne.heartbeat else {
                continue;
            };
            let online =
                now.mono_ms.saturating_sub(ne.last_seen_mono_ms) <= lease_ms.saturating_mul(2);
            let current_job = job_held_by(&inner, name).and_then(|id| JobId::parse(&id).ok());
            let mut cordons = Vec::new();
            for ((n, p), until) in &inner.cordons {
                if n == name && *until > now.mono_ms {
                    cordons.push(p.clone());
                }
            }
            cordons.sort();
            cordons.dedup();
            let node_name =
                NodeName::parse(name).map_err(|e| StoreError::Internal(e.to_string()))?;
            out.push(NodeRecord {
                name: node_name,
                last_seen: ne.last_seen_wall,
                online,
                draining: ne.draining,
                heartbeat: hb.clone(),
                current_job,
                cordons,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn job_counts(&self, now: Now, lease_ms: u64) -> Result<JobCounts, StoreError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| StoreError::Internal("mutex poisoned".into()))?;
        let mut jobs: HashMap<JobState, u64> = HashMap::new();
        for e in inner.jobs.values() {
            *jobs.entry(e.rec.state).or_insert(0) += 1;
        }
        let mut nodes_online = 0u64;
        let mut nodes_drained = 0u64;
        let mut total = 0u64;
        let oldest_pending_at = inner
            .jobs
            .values()
            .filter(|e| matches!(e.rec.state, JobState::Queued | JobState::Blocked))
            .map(|e| e.rec.created_at)
            .min();
        for e in inner.nodes.values() {
            // match nodes(): only nodes that have sent a heartbeat exist
            if e.heartbeat.is_none() {
                continue;
            }
            total += 1;
            if now.mono_ms.saturating_sub(e.last_seen_mono_ms) <= lease_ms.saturating_mul(2) {
                nodes_online += 1;
            }
            if e.draining {
                nodes_drained += 1;
            }
        }
        Ok(JobCounts {
            jobs: JobState::ALL
                .into_iter()
                .map(|s| (s, jobs.get(&s).copied().unwrap_or(0)))
                .collect(),
            nodes: total,
            nodes_online,
            nodes_drained,
            completed_done: jobs.get(&JobState::Done).copied().unwrap_or(0),
            completed_failed: jobs.get(&JobState::Failed).copied().unwrap_or(0),
            completed_cancelled: jobs.get(&JobState::Cancelled).copied().unwrap_or(0),
            oldest_pending_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryStore;

    #[test]
    fn conformance() {
        crate::conformance::run_all(MemoryStore::new);
    }
}
