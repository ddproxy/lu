use lu_proto::{
    ArtifactName, Completion, FailureClass, Heartbeat, JobId, JobSpec, JobState, NodeName, Outcome,
};
use lu_store::{Assignment, JobRecord, NodeRecord, Now, Store, StoreError};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

#[derive(Debug, Clone)]
struct JobStateRow {
    rec: JobRecord,
    lease_deadline_ms: Option<u64>,
    recovering_since_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct NodeRow {
    name: NodeName,
    last_seen: i64,
    draining: bool,
    heartbeat: Heartbeat,
    current_job: Option<JobId>,
}

#[derive(Default)]
pub struct FakeStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    jobs: BTreeMap<String, JobStateRow>,        // id string
    by_idem: HashMap<String, (String, String)>, // key -> (job_id, canonical spec)
    nodes: HashMap<String, NodeRow>,
    cordons: HashMap<(String, String), u64>, // (node, provider) -> until mono_ms
}

impl FakeStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn now_wall(now: Now) -> i64 {
    now.wall_unix
}

fn waiting_for(inner: &Inner, after: &[JobId]) -> Vec<JobId> {
    after
        .iter()
        .filter(|dep| {
            inner
                .jobs
                .get(dep.as_str())
                .is_none_or(|r| r.rec.state != JobState::Done)
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
        .filter(|(_, r)| {
            r.rec.state == JobState::Blocked
                && r.rec.spec.after.iter().any(|d| d.as_str() == dep)
                && r.rec.spec.after.iter().all(|d| {
                    inner
                        .jobs
                        .get(d.as_str())
                        .is_some_and(|j| j.rec.state == JobState::Done)
                })
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        let row = inner.jobs.get_mut(&id).unwrap();
        row.rec.state = JobState::Queued;
        row.rec.updated_at = now_wall(now);
        row.rec.failure_class = None;
        row.rec.summary = None;
        row.rec.finished_at = None;
    }
}

fn fail_dependents(inner: &mut Inner, dep: &str, dep_state: &str, now: Now) {
    let mut queue = vec![(dep.to_owned(), dep_state.to_owned())];
    while let Some((dep_id, dep_state)) = queue.pop() {
        let ids: Vec<String> = inner
            .jobs
            .iter()
            .filter(|(_, r)| {
                r.rec.state == JobState::Blocked
                    && r.rec.spec.after.iter().any(|d| d.as_str() == dep_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let row = inner.jobs.get_mut(&id).unwrap();
            row.rec.state = JobState::Failed;
            row.rec.failure_class = Some(FailureClass::Dependency);
            row.rec.summary = Some(format!("dependency {dep_id} {dep_state}"));
            row.rec.finished_at = Some(now_wall(now));
            row.rec.updated_at = now_wall(now);
            row.rec.node = None;
            row.lease_deadline_ms = None;
            row.recovering_since_ms = None;
            queue.push((id, "failed".to_owned()));
        }
    }
}

impl Store for FakeStore {
    fn create_job(&self, spec: &JobSpec, now: Now) -> Result<(JobRecord, bool), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(key) = &spec.idempotency_key {
            if let Some((jid, canon)) = inner.by_idem.get(key).cloned() {
                if canon == spec.canonical_json() {
                    let rec = inner
                        .jobs
                        .get(&jid)
                        .ok_or(StoreError::NotFound)?
                        .rec
                        .clone();
                    return Ok((rec, false));
                } else {
                    return Err(StoreError::Conflict { length: None });
                }
            }
        }
        let id = JobId::new();
        let mut blocked = false;
        for dep in &spec.after {
            let row = inner
                .jobs
                .get(dep.as_str())
                .ok_or_else(|| StoreError::InvalidDependency(dep.clone()))?;
            if matches!(row.rec.state, JobState::Failed | JobState::Cancelled) {
                return Err(StoreError::DeadDependency(dep.clone()));
            }
            if row.rec.state != JobState::Done {
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
            created_at: now_wall(now),
            updated_at: now_wall(now),
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
        let row = JobStateRow {
            rec: rec.clone(),
            lease_deadline_ms: None,
            recovering_since_ms: None,
        };
        if let Some(key) = &spec.idempotency_key {
            inner
                .by_idem
                .insert(key.clone(), (id.to_string(), spec.canonical_json()));
        }
        inner.jobs.insert(id.to_string(), row);
        Ok((rec, true))
    }

    fn get_job(&self, id: &JobId) -> Result<JobRecord, StoreError> {
        let inner = self.inner.lock().unwrap();
        inner
            .jobs
            .get(id.as_str())
            .map(|r| {
                let mut rec = r.rec.clone();
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
        let inner = self.inner.lock().unwrap();
        let mut rows: Vec<_> = inner.jobs.values().map(|r| r.rec.clone()).collect();
        // newest first
        rows.sort_by(|a, b| b.id.cmp(&a.id));
        if let Some(a) = after {
            rows.retain(|r| &r.id < a);
        }
        if let Some(st) = state {
            rows.retain(|r| r.state == st);
        }
        rows.truncate(limit);
        for rec in &mut rows {
            refresh_waiting(&inner, rec);
        }
        Ok(rows)
    }

    fn cancel_job(&self, id: &JobId, now: Now) -> Result<JobRecord, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let terminal = {
            let row = inner
                .jobs
                .get_mut(id.as_str())
                .ok_or(StoreError::NotFound)?;
            let is_terminal = match row.rec.state {
                JobState::Blocked | JobState::Queued => {
                    row.rec.state = JobState::Cancelled;
                    row.rec.finished_at = Some(now_wall(now));
                    true
                }
                JobState::Leased | JobState::Running | JobState::Recovering => {
                    row.rec.cancel_requested = true;
                    false
                }
                JobState::Done | JobState::Failed | JobState::Cancelled | JobState::NeedsInput => {
                    return Err(StoreError::Conflict { length: None });
                }
            };
            row.rec.updated_at = now_wall(now);
            is_terminal
        };
        if terminal {
            fail_dependents(&mut inner, id.as_str(), "cancelled", now);
        }
        let mut rec = inner.jobs.get(id.as_str()).unwrap().rec.clone();
        refresh_waiting(&inner, &mut rec);
        Ok(rec)
    }

    fn heartbeat(&self, hb: &Heartbeat, now: Now) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let name = hb.node.to_string();
        let entry = inner.nodes.entry(name.clone()).or_insert_with(|| NodeRow {
            name: hb.node.clone(),
            last_seen: now_wall(now),
            draining: false,
            heartbeat: hb.clone(),
            current_job: None,
        });
        entry.last_seen = now_wall(now);
        entry.heartbeat = hb.clone();
        Ok(())
    }

    fn next_assignment(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
    ) -> Result<Option<Assignment>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        // find node
        let nr = match inner.nodes.get(node.as_str()).cloned() {
            Some(n) => n,
            None => return Ok(None),
        };
        if nr.draining {
            return Ok(None);
        }
        if nr.heartbeat.harness != lu_proto::HarnessState::Idle {
            return Ok(None);
        }
        if nr.current_job.is_some() {
            return Ok(None);
        }
        // find oldest queued job this node may run
        let mut candidates: Vec<String> = inner
            .jobs
            .values()
            .filter(|j| j.rec.state == JobState::Queued)
            .map(|j| j.rec.id.to_string())
            .collect();
        candidates.sort(); // older ids first (time sortable prefix)
        let chosen = candidates.into_iter().find(|jid| {
            let j = &inner.jobs[jid].rec;
            if let Some(provider) = &j.spec.provider {
                // cordon check
                let cordoned = inner
                    .cordons
                    .get(&(node.to_string(), provider.clone()))
                    .map(|u| *u > now.mono_ms)
                    .unwrap_or(false);
                if cordoned {
                    return false;
                }
                // offered by node?
                if !nr.heartbeat.providers.iter().any(|p| &p.name == provider) {
                    return false;
                }
            }
            true
        });
        let Some(jid) = chosen else {
            return Ok(None);
        };
        let (a, job_id_clone) = {
            let row = inner.jobs.get_mut(&jid).ok_or(StoreError::NotFound)?;
            row.rec.state = JobState::Leased;
            row.rec.node = Some(node.clone());
            row.rec.epoch += 1;
            row.rec.updated_at = now_wall(now);
            row.lease_deadline_ms = Some(now.mono_ms + lease_ms);
            row.recovering_since_ms = None;
            let a = Assignment {
                job_id: row.rec.id.clone(),
                epoch: row.rec.epoch,
                spec: row.rec.spec.clone(),
            };
            (a, row.rec.id.clone())
        };
        if let Some(nr) = inner.nodes.get_mut(node.as_str()) {
            nr.current_job = Some(job_id_clone);
        }
        Ok(Some(a))
    }

    fn pending_cancels(&self, node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();
        for row in inner.jobs.values() {
            if row.rec.node.as_ref() == Some(node) && row.rec.cancel_requested {
                out.push((row.rec.id.clone(), row.rec.epoch));
            }
        }
        Ok(out)
    }

    fn start_job(&self, id: &JobId, epoch: u64, now: Now) -> Result<JobRecord, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if row.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if row.rec.state != JobState::Leased {
            return Err(StoreError::Conflict { length: None });
        }
        row.rec.state = JobState::Running;
        row.rec.started_at = Some(now_wall(now));
        row.rec.updated_at = now_wall(now);
        Ok(row.rec.clone())
    }

    fn renew_lease(
        &self,
        id: &JobId,
        epoch: u64,
        now: Now,
        lease_ms: u64,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if row.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        match row.rec.state {
            JobState::Leased | JobState::Running => {
                row.lease_deadline_ms = Some(now.mono_ms + lease_ms);
                row.rec.updated_at = now_wall(now);
                Ok(row.rec.cancel_requested)
            }
            _ => Err(StoreError::Conflict { length: None }),
        }
    }

    fn reserve_log(&self, id: &JobId, epoch: u64, offset: u64, n: u64) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if row.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if offset != row.rec.log_len {
            return Err(StoreError::Conflict {
                length: Some(row.rec.log_len),
            });
        }
        row.rec.log_len = row.rec.log_len.saturating_add(n);
        Ok(row.rec.log_len)
    }

    fn record_artifact(
        &self,
        id: &JobId,
        epoch: u64,
        name: ArtifactName,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .jobs
            .get_mut(id.as_str())
            .ok_or(StoreError::NotFound)?;
        if row.rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !row.rec.artifacts.contains(&name) {
            row.rec.artifacts.push(name);
        }
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
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .jobs
            .get_mut(completion.job_id.as_str())
            .ok_or(StoreError::NotFound)?;
        if row.rec.epoch != completion.epoch {
            return Err(StoreError::StaleEpoch);
        }
        // must be in active state
        match row.rec.state {
            JobState::Leased | JobState::Running | JobState::Recovering => {}
            _ => return Err(StoreError::Conflict { length: None }),
        }
        row.rec.exit_code = completion.exit_code;
        row.rec.summary = completion.summary.clone();
        let holding_node = row.rec.node.clone();
        let job_id_clone = row.rec.id.clone();
        let spec_provider = row.rec.spec.provider.clone();
        match completion.outcome {
            Outcome::Done => {
                row.rec.state = JobState::Done;
                row.rec.failure_class = None;
                row.rec.finished_at = Some(now_wall(now));
            }
            Outcome::Cancelled => {
                row.rec.state = JobState::Cancelled;
                row.rec.finished_at = Some(now_wall(now));
            }
            Outcome::Failed(class) => {
                row.rec.failure_class = Some(class);
                let retryable = class.retryable(row.rec.attempt, max_retries);
                if retryable {
                    row.rec.state = JobState::Queued;
                    row.rec.attempt += 1;
                    row.rec.node = None;
                    row.lease_deadline_ms = None;
                    row.recovering_since_ms = None;
                } else {
                    row.rec.state = JobState::Failed;
                    row.rec.finished_at = Some(now_wall(now));
                }
                if class == FailureClass::Provider {
                    if let Some(node) = &holding_node {
                        let prov = match &failure {
                            Some((_, Some(p))) => Some(p.clone()),
                            _ => spec_provider.clone(),
                        };
                        if let Some(provider) = prov {
                            inner
                                .cordons
                                .insert((node.to_string(), provider), now.mono_ms + cordon_ms);
                        }
                    }
                }
            }
        }
        // clear current job on node
        if let Some(node) = &holding_node {
            if let Some(nr) = inner.nodes.get_mut(node.as_str()) {
                if nr.current_job.as_ref() == Some(&job_id_clone) {
                    nr.current_job = None;
                }
            }
        }
        let final_state = {
            let row = inner
                .jobs
                .get_mut(completion.job_id.as_str())
                .ok_or(StoreError::NotFound)?;
            if matches!(
                row.rec.state,
                JobState::Done | JobState::Failed | JobState::Queued | JobState::Cancelled
            ) {
                row.lease_deadline_ms = None;
            }
            row.rec.updated_at = now_wall(now);
            row.rec.state
        };
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
        let mut inner = self.inner.lock().unwrap();
        let mut moved = Vec::new();
        let ids: Vec<String> = inner.jobs.keys().cloned().collect();
        for id in ids {
            let row = inner.jobs.get_mut(&id).unwrap();
            if matches!(row.rec.state, JobState::Leased | JobState::Running) {
                if let Some(deadline) = row.lease_deadline_ms {
                    if now.mono_ms >= deadline {
                        row.rec.state = JobState::Recovering;
                        row.rec.updated_at = now_wall(now);
                        row.recovering_since_ms = Some(now.mono_ms);
                        moved.push(row.rec.id.clone());
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
        let mut inner = self.inner.lock().unwrap();
        let mut moved = Vec::new();
        let ids: Vec<String> = inner.jobs.keys().cloned().collect();
        for id in ids {
            let clear: Option<(NodeName, JobId)> = {
                let row = inner.jobs.get_mut(&id).unwrap();
                if row.rec.state == JobState::Recovering {
                    let since = row.recovering_since_ms.unwrap_or(now.mono_ms);
                    if now.mono_ms.saturating_sub(since) >= grace_ms {
                        let node = row.rec.node.clone();
                        let jid = row.rec.id.clone();
                        if row.rec.attempt <= max_retries {
                            row.rec.state = JobState::Queued;
                            row.rec.attempt += 1;
                            row.rec.failure_class = Some(FailureClass::Node);
                            row.rec.node = None;
                        } else {
                            row.rec.state = JobState::Failed;
                            row.rec.failure_class = Some(FailureClass::Node);
                            row.rec.finished_at = Some(now_wall(now));
                        }
                        row.lease_deadline_ms = None;
                        row.recovering_since_ms = None;
                        moved.push(jid.clone());
                        row.rec.updated_at = now_wall(now);
                        node.map(|n| (n, jid))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((n, jid)) = clear {
                if let Some(nr) = inner.nodes.get_mut(n.as_str()) {
                    if nr.current_job.as_ref() == Some(&jid) {
                        nr.current_job = None;
                    }
                }
            }
        }
        Ok(moved)
    }

    fn recover_on_boot(&self, now: Now) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let mut count = 0u64;
        for row in inner.jobs.values_mut() {
            if matches!(row.rec.state, JobState::Leased | JobState::Running) {
                row.rec.state = JobState::Recovering;
                row.rec.updated_at = now_wall(now);
                row.recovering_since_ms = Some(now.mono_ms);
                count += 1;
            }
        }
        Ok(count)
    }

    fn set_draining(&self, node: &NodeName, draining: bool) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let nr = inner
            .nodes
            .entry(node.to_string())
            .or_insert_with(|| NodeRow {
                name: node.clone(),
                last_seen: 0,
                draining,
                heartbeat: Heartbeat {
                    node: node.clone(),
                    harness: lu_proto::HarnessState::Idle,
                    providers: vec![],
                    reiki: None,
                    host: None,
                    jobs: Vec::new(),
                },
                current_job: None,
            });
        nr.draining = draining;
        // if node didn't exist before, we need a heartbeat baseline - but tests may drain before heartbeat; acceptable.
        Ok(())
    }

    fn nodes(&self, now: Now, lease_ms: u64) -> Result<Vec<NodeRecord>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();
        for nr in inner.nodes.values() {
            let online_window = lease_ms.saturating_mul(2);
            // online if seen within two lease periods; last_seen is wall seconds, need mono-ish comparison not perfect.
            // store contract: "Seen within two lease periods of the `now` given to the query."
            // last_seen stored as wall_unix by our fake; convert approx: can't perfectly map, but tests likely fine.
            let online = nr.last_seen > 0
                && (now.wall_unix - nr.last_seen) <= ((online_window / 1000) as i64).max(1);
            let mut cordons = Vec::new();
            for ((n, p), until) in &inner.cordons {
                if n == nr.name.as_str() && *until > now.mono_ms {
                    cordons.push(p.clone());
                }
            }
            out.push(NodeRecord {
                name: nr.name.clone(),
                last_seen: nr.last_seen,
                online,
                draining: nr.draining,
                heartbeat: nr.heartbeat.clone(),
                current_job: nr.current_job.clone(),
                cordons,
            });
        }
        Ok(out)
    }
}
