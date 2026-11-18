//! SQLite-backed [`crate::Store`].

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use lu_proto::{
    ArtifactName, CancelEffect, Completion, FailureClass, Heartbeat, JobId, JobSpec, JobState,
    NodeName, Outcome, MAX_LOG_BYTES,
};

use crate::{Assignment, JobCounts, JobRecord, NodeRecord, Now, Store, StoreError};

const CURRENT_SCHEMA_VERSION: i64 = crate::SCHEMA_VERSION;

fn internal<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Internal(e.to_string())
}

fn state_str(s: JobState) -> &'static str {
    match s {
        JobState::Blocked => "blocked",
        JobState::Queued => "queued",
        JobState::Leased => "leased",
        JobState::Running => "running",
        JobState::Recovering => "recovering",
        JobState::NeedsInput => "needs_input",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

fn parse_state(s: &str) -> Result<JobState, StoreError> {
    Ok(match s {
        "blocked" => JobState::Blocked,
        "queued" => JobState::Queued,
        "leased" => JobState::Leased,
        "running" => JobState::Running,
        "recovering" => JobState::Recovering,
        "needs_input" => JobState::NeedsInput,
        "done" => JobState::Done,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        _ => return Err(internal(format!("unknown state {s}"))),
    })
}

fn failure_str(f: FailureClass) -> &'static str {
    match f {
        FailureClass::Node => "node",
        FailureClass::Provider => "provider",
        FailureClass::Execution => "execution",
        FailureClass::Dependency => "dependency",
    }
}

fn parse_failure(s: &str) -> Result<FailureClass, StoreError> {
    Ok(match s {
        "node" => FailureClass::Node,
        "provider" => FailureClass::Provider,
        "execution" => FailureClass::Execution,
        "dependency" => FailureClass::Dependency,
        _ => return Err(internal(format!("unknown failure class {s}"))),
    })
}

fn artifact_str(a: ArtifactName) -> &'static str {
    match a {
        ArtifactName::DiffPatch => "diff.patch",
        ArtifactName::RunLog => "run.log",
        ArtifactName::ReportJson => "report.json",
    }
}

fn parse_artifact(s: &str) -> Result<ArtifactName, StoreError> {
    Ok(match s {
        "diff.patch" => ArtifactName::DiffPatch,
        "run.log" => ArtifactName::RunLog,
        "report.json" => ArtifactName::ReportJson,
        _ => return Err(internal(format!("unknown artifact {s}"))),
    })
}

fn encode_artifacts(arts: &[ArtifactName]) -> String {
    let v: Vec<&str> = arts.iter().map(|a| artifact_str(*a)).collect();
    serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_owned())
}

fn decode_artifacts(s: &str) -> Result<Vec<ArtifactName>, StoreError> {
    let v: Vec<String> = serde_json::from_str(s).map_err(internal)?;
    v.into_iter().map(|x| parse_artifact(&x)).collect()
}

fn decode_spec(s: &str) -> Result<JobSpec, StoreError> {
    let spec: JobSpec =
        serde_json::from_str(s).map_err(|e| internal(format!("invalid stored spec: {e}")))?;
    spec.validate()
        .map_err(|e| internal(format!("invalid stored spec: {e}")))?;
    Ok(spec)
}

fn check_transition(from: JobState, to: JobState) -> Result<(), StoreError> {
    if JobState::can_transition(from, to) {
        Ok(())
    } else {
        Err(StoreError::Conflict { length: None })
    }
}

/// Persistent store backed by a single SQLite database file.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Mutex<Connection>,
    _path: PathBuf,
    /// Shared advisory lock held for the store's lifetime so `lu restore` can refuse a live database.
    _lock: std::fs::File,
}

impl SqliteStore {
    /// Opens (and creates/migrates) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        // Opening a path in a missing directory is an error, not a panic.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(internal(format!(
                    "database directory does not exist: {}",
                    parent.display()
                )));
            }
        }
        // create file 0600 if absent
        if !path.exists() {
            let f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(internal)?;
            drop(f);
        } else {
            // best effort enforce 0600 on existing? "creates the file 0600" - ensure perms reasonable
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(internal)?;
        lock.try_lock_shared()
            .map_err(|error| internal(format!("database {} is in use: {error}", path.display())))?;
        let conn = Connection::open(path).map_err(internal)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(internal)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(internal)?;
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(internal)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(internal)?;
        let store = Self {
            conn: Mutex::new(conn),
            _path: path.to_path_buf(),
            _lock: lock,
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );",
        )
        .map_err(internal)?;
        let ver: Option<i64> = tx
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(internal)?;
        match ver {
            Some(v) if v > CURRENT_SCHEMA_VERSION => {
                return Err(internal(format!(
                    "database schema version {v} is newer than supported {CURRENT_SCHEMA_VERSION}"
                )));
            }
            Some(v) if v == CURRENT_SCHEMA_VERSION => {
                tx.commit().map_err(internal)?;
                return Ok(());
            }
            Some(v) => {
                if v < 1 {
                    apply_v1(&tx)?;
                }
                if v < 2 {
                    apply_v2(&tx)?;
                }
                if v < 3 {
                    apply_v3(&tx)?;
                }
                tx.execute("DELETE FROM schema_version", [])
                    .map_err(internal)?;
                tx.execute(
                    "INSERT INTO schema_version(version) VALUES (?1)",
                    params![CURRENT_SCHEMA_VERSION],
                )
                .map_err(internal)?;
            }
            None => {
                apply_v1(&tx)?;
                apply_v2(&tx)?;
                apply_v3(&tx)?;
                tx.execute(
                    "INSERT INTO schema_version(version) VALUES (?1)",
                    params![CURRENT_SCHEMA_VERSION],
                )
                .map_err(internal)?;
            }
        }
        tx.commit().map_err(internal)?;
        Ok(())
    }
}

fn apply_v1(tx: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS jobs (
            id TEXT PRIMARY KEY,
            state TEXT NOT NULL,
            spec_json TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            node TEXT,
            epoch INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            started_at INTEGER,
            finished_at INTEGER,
            failure_class TEXT,
            summary TEXT,
            exit_code INTEGER,
            artifacts_json TEXT NOT NULL,
            cancel_requested INTEGER NOT NULL,
            log_len INTEGER NOT NULL,
            lease_until_mono_ms INTEGER,
            recovering_since_mono_ms INTEGER
        );
        CREATE TABLE IF NOT EXISTS nodes (
            name TEXT PRIMARY KEY,
            heartbeat_json TEXT NOT NULL,
            last_seen_wall INTEGER NOT NULL,
            last_seen_mono_ms INTEGER NOT NULL,
            draining INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS cordons (
            node TEXT NOT NULL,
            provider TEXT NOT NULL,
            until_mono_ms INTEGER NOT NULL,
            PRIMARY KEY(node, provider)
        );
        CREATE TABLE IF NOT EXISTS idempotency (
            key TEXT PRIMARY KEY,
            spec_hash TEXT NOT NULL,
            job_id TEXT NOT NULL
        );",
    )
    .map_err(internal)?;
    Ok(())
}

fn apply_v2(tx: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS job_deps (
            job TEXT NOT NULL,
            dep TEXT NOT NULL,
            ord INTEGER NOT NULL,
            PRIMARY KEY(job, dep),
            FOREIGN KEY(job) REFERENCES jobs(id),
            FOREIGN KEY(dep) REFERENCES jobs(id)
        );
        CREATE INDEX IF NOT EXISTS job_deps_dep ON job_deps(dep);",
    )
    .map_err(internal)?;
    Ok(())
}

fn apply_v3(tx: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    // Reiki-derived cordons yield to live health, so they also need a streak counter. Rows from
    // older databases keep their time-based behaviour (both columns default to false/zero).
    tx.execute_batch(
        "ALTER TABLE cordons ADD COLUMN reiki_derived INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE cordons ADD COLUMN healthy_hb_count INTEGER NOT NULL DEFAULT 0;",
    )
    .map_err(internal)?;
    Ok(())
}

fn load_waiting(conn: &Connection, id: &str) -> Result<Vec<JobId>, StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT d.dep FROM job_deps d JOIN jobs j ON j.id=d.dep
             WHERE d.job=?1 AND j.state!='done' ORDER BY d.ord",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![id], |r| r.get::<_, String>(0))
        .map_err(internal)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(JobId::parse(&row.map_err(internal)?).map_err(internal)?);
    }
    Ok(out)
}

fn fail_dependents(
    tx: &rusqlite::Transaction<'_>,
    dep: &str,
    dep_state: &str,
    now: Now,
) -> Result<(), StoreError> {
    let mut queue = vec![(dep.to_owned(), dep_state.to_owned())];
    while let Some((dep_id, dep_state)) = queue.pop() {
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT j.id FROM jobs j JOIN job_deps d ON d.job=j.id
                     WHERE d.dep=?1 AND j.state='blocked'",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![dep_id], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut v = Vec::new();
            for row in rows {
                v.push(row.map_err(internal)?);
            }
            v
        };
        for id in ids {
            let summary = format!("dependency {dep_id} {dep_state}");
            tx.execute(
                "UPDATE jobs SET state='failed',failure_class='dependency',summary=?2,
                 finished_at=?3,updated_at=?3,node=NULL,lease_until_mono_ms=NULL,
                 recovering_since_mono_ms=NULL,cancel_requested=0 WHERE id=?1",
                params![id, summary, now.wall_unix],
            )
            .map_err(internal)?;
            queue.push((id, "failed".to_owned()));
        }
    }
    Ok(())
}

fn get_job_tx(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
) -> Result<(JobRecord, Option<u64>, Option<u64>), StoreError> {
    let mut stmt = tx
        .prepare("SELECT id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,lease_until_mono_ms,recovering_since_mono_ms FROM jobs WHERE id=?1")
        .map_err(internal)?;
    let row = stmt
        .query_row(params![id], |r| {
            Ok((
                r.get::<_, String>("id")?,
                r.get::<_, String>("state")?,
                r.get::<_, String>("spec_json")?,
                r.get::<_, u32>("attempt")?,
                r.get::<_, Option<String>>("node")?,
                r.get::<_, i64>("epoch")?,
                r.get::<_, i64>("created_at")?,
                r.get::<_, i64>("updated_at")?,
                r.get::<_, Option<i64>>("started_at")?,
                r.get::<_, Option<i64>>("finished_at")?,
                r.get::<_, Option<String>>("failure_class")?,
                r.get::<_, Option<String>>("summary")?,
                r.get::<_, Option<i32>>("exit_code")?,
                r.get::<_, String>("artifacts_json")?,
                r.get::<_, i64>("cancel_requested")?,
                r.get::<_, i64>("log_len")?,
                r.get::<_, Option<i64>>("lease_until_mono_ms")?,
                r.get::<_, Option<i64>>("recovering_since_mono_ms")?,
            ))
        })
        .optional()
        .map_err(internal)?
        .ok_or(StoreError::NotFound)?;
    // reconstruct via manual to also get lease fields
    let (
        id_s,
        state_s,
        spec_json,
        attempt,
        node_s,
        epoch_i,
        created_at,
        updated_at,
        started_at,
        finished_at,
        failure_s,
        summary,
        exit_code,
        artifacts_json,
        cancel_i,
        log_len_i,
        lease_until,
        recovering_since,
    ) = row;
    let idp = JobId::parse(&id_s).map_err(internal)?;
    let state = parse_state(&state_s)?;
    let spec = decode_spec(&spec_json)?;
    let node = match node_s {
        Some(n) => Some(NodeName::parse(&n).map_err(internal)?),
        None => None,
    };
    let failure_class = match failure_s {
        Some(f) => Some(parse_failure(&f)?),
        None => None,
    };
    let artifacts = decode_artifacts(&artifacts_json)?;
    let rec = JobRecord {
        id: idp,
        state,
        spec,
        attempt,
        node,
        epoch: epoch_i as u64,
        created_at,
        updated_at,
        started_at,
        finished_at,
        failure_class,
        summary,
        exit_code,
        artifacts,
        cancel_requested: cancel_i != 0,
        log_len: log_len_i as u64,
        waiting_on: load_waiting(tx, &id_s)?,
    };
    Ok((
        rec,
        lease_until.map(|v| v as u64),
        recovering_since.map(|v| v as u64),
    ))
}

fn update_job_tx(
    tx: &rusqlite::Transaction<'_>,
    rec: &JobRecord,
    lease_until: Option<u64>,
    recovering_since: Option<u64>,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE jobs SET state=?2,spec_json=?3,attempt=?4,node=?5,epoch=?6,created_at=?7,updated_at=?8,started_at=?9,finished_at=?10,failure_class=?11,summary=?12,exit_code=?13,artifacts_json=?14,cancel_requested=?15,log_len=?16,lease_until_mono_ms=?17,recovering_since_mono_ms=?18 WHERE id=?1",
        params![
            rec.id.as_str(),
            state_str(rec.state),
            rec.spec.canonical_json(),
            rec.attempt as i64,
            rec.node.as_ref().map(|n| n.as_str()),
            rec.epoch as i64,
            rec.created_at,
            rec.updated_at,
            rec.started_at,
            rec.finished_at,
            rec.failure_class.map(failure_str),
            rec.summary,
            rec.exit_code,
            encode_artifacts(&rec.artifacts),
            if rec.cancel_requested { 1i64 } else { 0i64 },
            rec.log_len as i64,
            lease_until.map(|v| v as i64),
            recovering_since.map(|v| v as i64),
        ],
    ).map_err(internal)?;
    Ok(())
}

fn node_offers_provider(hb: &Heartbeat, provider: &str) -> bool {
    hb.providers
        .iter()
        .any(|p| p.name == provider && matches!(p.state, lu_proto::ProviderState::Ok))
}

fn refresh_reiki_cordons(tx: &rusqlite::Transaction<'_>, hb: &Heartbeat) -> Result<(), StoreError> {
    let cordons: Vec<(String, u64)> = {
        let mut stmt = tx
            .prepare(
                "SELECT provider, healthy_hb_count FROM cordons WHERE node=?1 AND reiki_derived=1",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![hb.node.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })
            .map_err(internal)?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row.map_err(internal)?);
        }
        v
    };
    for (provider, count) in cordons {
        let healthy = node_offers_provider(hb, &provider);
        if healthy && count.saturating_add(1) >= 2 {
            tx.execute(
                "DELETE FROM cordons WHERE node=?1 AND provider=?2 AND reiki_derived=1",
                params![hb.node.as_str(), provider],
            )
            .map_err(internal)?;
        } else {
            let next = if healthy { count.saturating_add(1) } else { 0 };
            tx.execute(
                "UPDATE cordons SET healthy_hb_count=?3 WHERE node=?1 AND provider=?2 AND reiki_derived=1",
                params![hb.node.as_str(), provider, next as i64],
            )
            .map_err(internal)?;
        }
    }
    Ok(())
}

impl Store for SqliteStore {
    fn create_job(&self, spec: &JobSpec, now: Now) -> Result<(JobRecord, bool), StoreError> {
        spec.validate()
            .map_err(|e| StoreError::InvalidSpec(e.to_string()))?;
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let canonical = spec.canonical_json();
        if let Some(key) = &spec.idempotency_key {
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT spec_hash, job_id FROM idempotency WHERE key=?1",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(internal)?;
            if let Some((hash, job_id)) = existing {
                if hash == canonical {
                    let (rec, _, _) = get_job_tx(&tx, &job_id)?;
                    tx.commit().map_err(internal)?;
                    return Ok((rec, false));
                } else {
                    return Err(StoreError::Conflict { length: None });
                }
            }
        }
        let id = JobId::new();
        let mut blocked = false;
        for dep in &spec.after {
            let dep_state: Option<String> = tx
                .query_row(
                    "SELECT state FROM jobs WHERE id=?1",
                    params![dep.as_str()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(internal)?;
            match dep_state.as_deref() {
                Some("done") => {}
                Some("failed" | "cancelled") => {
                    return Err(StoreError::DeadDependency(dep.clone()))
                }
                Some(_) => blocked = true,
                None => return Err(StoreError::InvalidDependency(dep.clone())),
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
        tx.execute(
            "INSERT INTO jobs(id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,lease_until_mono_ms,recovering_since_mono_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                rec.id.as_str(), state_str(rec.state), canonical, 1i64, Option::<String>::None, 0i64,
                rec.created_at, rec.updated_at, Option::<i64>::None, Option::<i64>::None,
                Option::<String>::None, Option::<String>::None, Option::<i32>::None,
                encode_artifacts(&[]), 0i64, 0i64, Option::<i64>::None, Option::<i64>::None
            ],
        ).map_err(internal)?;
        for (ord, dep) in spec.after.iter().enumerate() {
            tx.execute(
                "INSERT INTO job_deps(job,dep,ord) VALUES (?1,?2,?3)",
                params![rec.id.as_str(), dep.as_str(), ord as i64],
            )
            .map_err(internal)?;
        }
        rec.waiting_on = load_waiting(&tx, rec.id.as_str())?;
        if let Some(key) = &spec.idempotency_key {
            tx.execute(
                "INSERT INTO idempotency(key,spec_hash,job_id) VALUES (?1,?2,?3)",
                params![key, canonical, rec.id.as_str()],
            )
            .map_err(internal)?;
        }
        tx.commit().map_err(internal)?;
        Ok((rec, true))
    }

    fn get_job(&self, id: &JobId) -> Result<JobRecord, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare("SELECT id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len FROM jobs WHERE id=?1")
            .map_err(internal)?;
        let rec = stmt
            .query_row(params![id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<i32>>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, i64>(15)?,
                ))
            })
            .optional()
            .map_err(internal)?
            .ok_or(StoreError::NotFound)?;
        let (
            id_s,
            state_s,
            spec_json,
            attempt,
            node_s,
            epoch_i,
            created_at,
            updated_at,
            started_at,
            finished_at,
            failure_s,
            summary,
            exit_code,
            artifacts_json,
            cancel_i,
            log_len_i,
        ) = rec;
        Ok(JobRecord {
            id: JobId::parse(&id_s).map_err(internal)?,
            state: parse_state(&state_s)?,
            spec: decode_spec(&spec_json)?,
            attempt,
            node: match node_s {
                Some(n) => Some(NodeName::parse(&n).map_err(internal)?),
                None => None,
            },
            epoch: epoch_i as u64,
            created_at,
            updated_at,
            started_at,
            finished_at,
            failure_class: match failure_s {
                Some(f) => Some(parse_failure(&f)?),
                None => None,
            },
            summary,
            exit_code,
            artifacts: decode_artifacts(&artifacts_json)?,
            cancel_requested: cancel_i != 0,
            log_len: log_len_i as u64,
            waiting_on: load_waiting(&conn, &id_s)?,
        })
    }

    fn list_jobs(
        &self,
        state: Option<JobState>,
        limit: usize,
        after: Option<&JobId>,
    ) -> Result<Vec<JobRecord>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut out = Vec::new();
        let base = "SELECT id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len FROM jobs";
        let mut sql = base.to_string();
        // build query per case to keep bound parameters simple.

        match (state, after) {
            (None, None) => {
                sql.push_str(" ORDER BY id DESC LIMIT ?1");
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map(params![limit as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, Option<i64>>(8)?,
                            row.get::<_, Option<i64>>(9)?,
                            row.get::<_, Option<String>>(10)?,
                            row.get::<_, Option<String>>(11)?,
                            row.get::<_, Option<i32>>(12)?,
                            row.get::<_, String>(13)?,
                            row.get::<_, i64>(14)?,
                            row.get::<_, i64>(15)?,
                        ))
                    })
                    .map_err(internal)?;
                for r in rows {
                    let t = r.map_err(internal)?;
                    let mut rec = tuple_to_job(t)?;
                    rec.waiting_on = load_waiting(&conn, rec.id.as_str())?;
                    out.push(rec);
                }
            }
            (Some(st), None) => {
                sql.push_str(" WHERE state=?1 ORDER BY id DESC LIMIT ?2");
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map(params![state_str(st), limit as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, Option<i64>>(8)?,
                            row.get::<_, Option<i64>>(9)?,
                            row.get::<_, Option<String>>(10)?,
                            row.get::<_, Option<String>>(11)?,
                            row.get::<_, Option<i32>>(12)?,
                            row.get::<_, String>(13)?,
                            row.get::<_, i64>(14)?,
                            row.get::<_, i64>(15)?,
                        ))
                    })
                    .map_err(internal)?;
                for r in rows {
                    let t = r.map_err(internal)?;
                    let mut rec = tuple_to_job(t)?;
                    rec.waiting_on = load_waiting(&conn, rec.id.as_str())?;
                    out.push(rec);
                }
            }
            (None, Some(a)) => {
                sql.push_str(" WHERE id < ?1 ORDER BY id DESC LIMIT ?2");
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map(params![a.as_str(), limit as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, Option<i64>>(8)?,
                            row.get::<_, Option<i64>>(9)?,
                            row.get::<_, Option<String>>(10)?,
                            row.get::<_, Option<String>>(11)?,
                            row.get::<_, Option<i32>>(12)?,
                            row.get::<_, String>(13)?,
                            row.get::<_, i64>(14)?,
                            row.get::<_, i64>(15)?,
                        ))
                    })
                    .map_err(internal)?;
                for r in rows {
                    let t = r.map_err(internal)?;
                    let mut rec = tuple_to_job(t)?;
                    rec.waiting_on = load_waiting(&conn, rec.id.as_str())?;
                    out.push(rec);
                }
            }
            (Some(st), Some(a)) => {
                sql.push_str(" WHERE state=?1 AND id < ?2 ORDER BY id DESC LIMIT ?3");
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map(params![state_str(st), a.as_str(), limit as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, Option<i64>>(8)?,
                            row.get::<_, Option<i64>>(9)?,
                            row.get::<_, Option<String>>(10)?,
                            row.get::<_, Option<String>>(11)?,
                            row.get::<_, Option<i32>>(12)?,
                            row.get::<_, String>(13)?,
                            row.get::<_, i64>(14)?,
                            row.get::<_, i64>(15)?,
                        ))
                    })
                    .map_err(internal)?;
                for r in rows {
                    let t = r.map_err(internal)?;
                    let mut rec = tuple_to_job(t)?;
                    rec.waiting_on = load_waiting(&conn, rec.id.as_str())?;
                    out.push(rec);
                }
            }
        }
        Ok(out)
    }

    fn cancel_job(&self, id: &JobId, now: Now) -> Result<JobRecord, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        match rec.state.cancel_effect() {
            CancelEffect::Now => {
                check_transition(rec.state, JobState::Cancelled)?;
                rec.state = JobState::Cancelled;
                rec.updated_at = now.wall_unix;
                rec.finished_at = Some(now.wall_unix);
                update_job_tx(&tx, &rec, None, None)?;
                fail_dependents(&tx, rec.id.as_str(), "cancelled", now)?;
            }
            CancelEffect::Request => {
                rec.cancel_requested = true;
                rec.updated_at = now.wall_unix;
                update_job_tx(&tx, &rec, lease, recov)?;
            }
            CancelEffect::Refuse => return Err(StoreError::Conflict { length: None }),
        }
        rec.waiting_on = load_waiting(&tx, rec.id.as_str())?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn heartbeat(&self, hb: &Heartbeat, now: Now) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let hb_json = serde_json::to_string(hb).map_err(internal)?;
        // preserve draining if exists
        let draining: Option<i64> = tx
            .query_row(
                "SELECT draining FROM nodes WHERE name=?1",
                params![hb.node.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        let draining = draining.unwrap_or(0);
        tx.execute(
            "INSERT INTO nodes(name,heartbeat_json,last_seen_wall,last_seen_mono_ms,draining) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(name) DO UPDATE SET heartbeat_json=excluded.heartbeat_json,last_seen_wall=excluded.last_seen_wall,last_seen_mono_ms=excluded.last_seen_mono_ms",
            params![hb.node.as_str(), hb_json, now.wall_unix, now.mono_ms as i64, draining],
        ).map_err(internal)?;
        refresh_reiki_cordons(&tx, hb)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn next_assignment(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
    ) -> Result<Option<Assignment>, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // node state
        let node_row: Option<(String, i64)> = tx
            .query_row(
                "SELECT heartbeat_json, draining FROM nodes WHERE name=?1",
                params![node.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let (hb_json, draining_i) = match node_row {
            Some(x) => x,
            None => return Ok(None),
        };
        if draining_i != 0 {
            return Ok(None);
        }
        let hb: Heartbeat = serde_json::from_str(&hb_json).map_err(internal)?;
        if !matches!(hb.harness, lu_proto::HarnessState::Idle) {
            return Ok(None);
        }
        // node must not hold a job
        let held: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM jobs WHERE node=?1 AND state IN ('leased','running') LIMIT 1",
                params![node.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        if held.is_some() {
            return Ok(None);
        }
        // oldest queued jobs
        let candidates: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT id FROM jobs WHERE state='queued' ORDER BY id ASC")
                .map_err(internal)?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut v = Vec::new();
            for id in ids {
                v.push(id.map_err(internal)?);
            }
            v
        };
        for jid in candidates {
            let (mut rec, _, _) = get_job_tx(&tx, &jid)?;
            if let Some(p) = &rec.spec.provider {
                // cordon check
                let cordoned: Option<i64> = tx
                    .query_row(
                        "SELECT until_mono_ms FROM cordons WHERE node=?1 AND provider=?2",
                        params![node.as_str(), p],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if let Some(until) = cordoned {
                    if (until as u64) > now.mono_ms {
                        continue;
                    }
                }
                if !node_offers_provider(&hb, p) {
                    continue;
                }
            }
            check_transition(rec.state, JobState::Leased)?;
            rec.state = JobState::Leased;
            rec.node = Some(node.clone());
            rec.epoch = rec.epoch.saturating_add(1);
            rec.updated_at = now.wall_unix;
            rec.failure_class = None;
            rec.summary = None;
            rec.exit_code = None;
            rec.finished_at = None;
            let lease_until = Some(now.mono_ms.saturating_add(lease_ms));
            update_job_tx(&tx, &rec, lease_until, None)?;
            tx.commit().map_err(internal)?;
            return Ok(Some(Assignment {
                job_id: rec.id,
                epoch: rec.epoch,
                spec: rec.spec,
            }));
        }
        tx.commit().map_err(internal)?;
        Ok(None)
    }

    fn pending_cancels(&self, node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, epoch FROM jobs WHERE node=?1 AND cancel_requested!=0 AND state IN ('leased','running')"
        ).map_err(internal)?;
        let rows = stmt
            .query_map(params![node.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            let (id, epoch) = r.map_err(internal)?;
            out.push((JobId::parse(&id).map_err(internal)?, epoch as u64));
        }
        Ok(out)
    }

    fn start_job(&self, id: &JobId, epoch: u64, now: Now) -> Result<JobRecord, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, _) = get_job_tx(&tx, id.as_str())?;
        if rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        check_transition(rec.state, JobState::Running)?;
        rec.state = JobState::Running;
        rec.updated_at = now.wall_unix;
        if rec.started_at.is_none() {
            rec.started_at = Some(now.wall_unix);
        }
        update_job_tx(&tx, &rec, lease, None)?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn renew_lease(
        &self,
        id: &JobId,
        epoch: u64,
        now: Now,
        lease_ms: u64,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, _, recov) = get_job_tx(&tx, id.as_str())?;
        if rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(
            rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        let mut recov = recov;
        if rec.state == JobState::Recovering {
            // The holder of the lease is alive: it resumes where it was.
            check_transition(rec.state, JobState::Running)?;
            rec.state = JobState::Running;
            recov = None;
        }
        rec.updated_at = now.wall_unix;
        let lease_until = Some(now.mono_ms.saturating_add(lease_ms));
        update_job_tx(&tx, &rec, lease_until, recov)?;
        tx.commit().map_err(internal)?;
        Ok(rec.cancel_requested)
    }

    fn reserve_log(&self, id: &JobId, epoch: u64, offset: u64, n: u64) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        if rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(
            rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        if offset != rec.log_len {
            return Err(StoreError::Conflict {
                length: Some(rec.log_len),
            });
        }
        let cap = MAX_LOG_BYTES;
        rec.log_len = if rec.log_len >= cap {
            cap
        } else {
            let want = rec.log_len.saturating_add(n);
            if want > cap {
                cap
            } else {
                want
            }
        };
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(rec.log_len)
    }

    fn record_artifact(
        &self,
        id: &JobId,
        epoch: u64,
        name: ArtifactName,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        if rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !rec.artifacts.contains(&name) {
            rec.artifacts.push(name);
        }
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
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
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, _lease, _recov) = get_job_tx(&tx, completion.job_id.as_str())?;
        if completion.epoch != rec.epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(
            rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        let node_for_cordon = rec.node.clone();
        rec.exit_code = completion.exit_code;
        rec.summary = completion.summary.clone();
        match completion.outcome {
            Outcome::Done => {
                check_transition(rec.state, JobState::Done)?;
                rec.state = JobState::Done;
                rec.finished_at = Some(now.wall_unix);
                rec.updated_at = now.wall_unix;
                rec.failure_class = None;
                update_job_tx(&tx, &rec, None, None)?;
                tx.execute(
                    "UPDATE jobs SET state='queued',updated_at=?2,failure_class=NULL,
                     summary=NULL,finished_at=NULL
                     WHERE state='blocked' AND id IN
                       (SELECT job FROM job_deps WHERE dep=?1)
                       AND NOT EXISTS
                       (SELECT 1 FROM job_deps d JOIN jobs j ON j.id=d.dep
                        WHERE d.job=jobs.id AND j.state!='done')",
                    params![rec.id.as_str(), now.wall_unix],
                )
                .map_err(internal)?;
            }
            Outcome::Cancelled => {
                check_transition(rec.state, JobState::Cancelled)?;
                rec.state = JobState::Cancelled;
                rec.finished_at = Some(now.wall_unix);
                rec.updated_at = now.wall_unix;
                update_job_tx(&tx, &rec, None, None)?;
                fail_dependents(&tx, rec.id.as_str(), "cancelled", now)?;
            }
            Outcome::Failed(out_class) => {
                let (class, provider) = failure.unwrap_or((out_class, None));
                rec.failure_class = Some(class);
                if class.retryable(rec.attempt, max_retries) {
                    check_transition(rec.state, JobState::Queued)?;
                    rec.state = JobState::Queued;
                    rec.attempt = rec.attempt.saturating_add(1);
                    rec.node = None;
                    rec.cancel_requested = false;
                    rec.finished_at = None;
                    rec.updated_at = now.wall_unix;
                    update_job_tx(&tx, &rec, None, None)?;
                    if class == FailureClass::Provider {
                        if let (Some(node), Some(p)) = (node_for_cordon, provider) {
                            let until = now.mono_ms.saturating_add(cordon_ms) as i64;
                            let reiki_derived = completion.retry_after_secs.is_some();
                            tx.execute(
                                "INSERT INTO cordons(node,provider,until_mono_ms,reiki_derived,healthy_hb_count) VALUES(?1,?2,?3,?4,0)
                                 ON CONFLICT(node,provider) DO UPDATE SET until_mono_ms=excluded.until_mono_ms,
                                 reiki_derived=excluded.reiki_derived, healthy_hb_count=0",
                                params![
                                    node.as_str(),
                                    p,
                                    until,
                                    if reiki_derived { 1i64 } else { 0i64 }
                                ],
                            ).map_err(internal)?;
                        }
                    }
                } else {
                    check_transition(rec.state, JobState::Failed)?;
                    rec.state = JobState::Failed;
                    rec.finished_at = Some(now.wall_unix);
                    rec.updated_at = now.wall_unix;
                    update_job_tx(&tx, &rec, None, None)?;
                    fail_dependents(&tx, rec.id.as_str(), "failed", now)?;
                }
            }
        }
        rec.waiting_on = load_waiting(&tx, rec.id.as_str())?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn expire_leases(&self, now: Now) -> Result<Vec<JobId>, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM jobs WHERE state IN ('leased','running') AND lease_until_mono_ms IS NOT NULL AND lease_until_mono_ms <= ?1").map_err(internal)?;
            let rows = stmt
                .query_map(params![now.mono_ms as i64], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r.map_err(internal)?);
            }
            v
        };
        let mut moved = Vec::new();
        for id in ids {
            let (mut rec, _, _) = get_job_tx(&tx, &id)?;
            check_transition(rec.state, JobState::Recovering)?;
            rec.state = JobState::Recovering;
            rec.updated_at = now.wall_unix;
            update_job_tx(&tx, &rec, None, Some(now.mono_ms))?;
            moved.push(rec.id);
        }
        tx.commit().map_err(internal)?;
        Ok(moved)
    }

    fn requeue_recovering(
        &self,
        now: Now,
        grace_ms: u64,
        max_retries: u32,
    ) -> Result<Vec<JobId>, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // Added up, not subtracted from `now`: a young process would clamp the cutoff to zero
        // and requeue a job that only just began recovering.
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM jobs WHERE state='recovering' AND (recovering_since_mono_ms IS NULL OR recovering_since_mono_ms + ?1 <= ?2)").map_err(internal)?;
            let rows = stmt
                .query_map(params![grace_ms as i64, now.mono_ms as i64], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(internal)?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r.map_err(internal)?);
            }
            v
        };
        let mut moved = Vec::new();
        for id in ids {
            let (mut rec, _, _) = get_job_tx(&tx, &id)?;
            // double-check grace
            // (query used cutoff; good enough)
            if FailureClass::Node.retryable(rec.attempt, max_retries) {
                check_transition(rec.state, JobState::Queued)?;
                rec.state = JobState::Queued;
                rec.attempt = rec.attempt.saturating_add(1);
                rec.node = None;
                rec.failure_class = None;
                rec.cancel_requested = false;
                rec.updated_at = now.wall_unix;
                rec.finished_at = None;
                update_job_tx(&tx, &rec, None, None)?;
            } else {
                check_transition(rec.state, JobState::Failed)?;
                rec.state = JobState::Failed;
                rec.failure_class = Some(FailureClass::Node);
                rec.updated_at = now.wall_unix;
                rec.finished_at = Some(now.wall_unix);
                update_job_tx(&tx, &rec, None, None)?;
                fail_dependents(&tx, rec.id.as_str(), "failed", now)?;
            }
            moved.push(rec.id);
        }
        tx.commit().map_err(internal)?;
        Ok(moved)
    }

    fn recover_on_boot(&self, now: Now) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT id FROM jobs WHERE state IN ('leased','running')")
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r.map_err(internal)?);
            }
            v
        };
        // The monotonic clock restarts with the process, so a stamp from the last run means
        // nothing now: start the grace over for jobs already recovering.
        tx.execute(
            "UPDATE jobs SET recovering_since_mono_ms=?1 WHERE state='recovering'",
            params![now.mono_ms as i64],
        )
        .map_err(internal)?;
        let mut count = 0u64;
        for id in ids {
            let (mut rec, _, _) = get_job_tx(&tx, &id)?;
            check_transition(rec.state, JobState::Recovering)?;
            rec.state = JobState::Recovering;
            rec.updated_at = now.wall_unix;
            update_job_tx(&tx, &rec, None, Some(now.mono_ms))?;
            count += 1;
        }
        tx.commit().map_err(internal)?;
        Ok(count)
    }

    fn set_draining(&self, node: &NodeName, draining: bool) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // ensure node row exists? if not, create minimal? draining without heartbeat - nodes() only returns nodes with heartbeat
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM nodes WHERE name=?1",
                params![node.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        if exists.is_none() {
            // The operator can only drain a node that has sent a heartbeat; anything else would
            // be a drain for a name that has not appeared yet.
            return Err(StoreError::NotFound);
        }
        tx.execute(
            "UPDATE nodes SET draining=?2 WHERE name=?1",
            params![node.as_str(), if draining { 1i64 } else { 0i64 }],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn nodes(&self, now: Now, lease_ms: u64) -> Result<Vec<NodeRecord>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn.prepare("SELECT name, heartbeat_json, last_seen_wall, last_seen_mono_ms, draining FROM nodes").map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            let (name, hb_json, last_seen_wall, last_seen_mono, draining_i) =
                r.map_err(internal)?;
            let heartbeat: Heartbeat = serde_json::from_str(&hb_json).map_err(internal)?;
            let online =
                now.mono_ms.saturating_sub(last_seen_mono as u64) <= lease_ms.saturating_mul(2);
            // current job
            let current: Option<String> = conn
                .query_row(
                    "SELECT id FROM jobs WHERE node=?1 AND state IN ('leased','running') LIMIT 1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(internal)?;
            let current_job = match current {
                Some(id) => Some(JobId::parse(&id).map_err(internal)?),
                None => None,
            };
            let mut cstmt = conn
                .prepare("SELECT provider FROM cordons WHERE node=?1 AND until_mono_ms > ?2")
                .map_err(internal)?;
            let crows = cstmt
                .query_map(params![name, now.mono_ms as i64], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut cordons = Vec::new();
            for c in crows {
                cordons.push(c.map_err(internal)?);
            }
            cordons.sort();
            cordons.dedup();
            out.push(NodeRecord {
                name: NodeName::parse(&name).map_err(internal)?,
                last_seen: last_seen_wall,
                online,
                draining: draining_i != 0,
                heartbeat,
                current_job,
                cordons,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn job_counts(&self, now: Now, lease_ms: u64) -> Result<JobCounts, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare("SELECT state, COUNT(*) FROM jobs GROUP BY state")
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(internal)?;
        let mut counts: HashMap<String, u64> = HashMap::new();
        for r in rows {
            let (s, c) = r.map_err(internal)?;
            counts.insert(s, c.max(0) as u64);
        }
        let mut job_counts = JobCounts {
            jobs: JobState::ALL
                .into_iter()
                .map(|s| (s, counts.get(state_str(s)).copied().unwrap_or(0)))
                .collect(),
            nodes: 0,
            nodes_online: 0,
            nodes_drained: 0,
            completed_done: 0,
            completed_failed: 0,
            completed_cancelled: 0,
            oldest_pending_at: None,
        };
        let nodes = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get::<_, i64>(0))
            .map_err(internal)?;
        job_counts.nodes = nodes.max(0) as u64;
        let online = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE last_seen_mono_ms + (?1 * 2) >= ?2",
                params![lease_ms as i64, now.mono_ms as i64],
                |r| r.get::<_, i64>(0),
            )
            .map_err(internal)?;
        job_counts.nodes_online = online.max(0) as u64;
        let drained = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE draining=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(internal)?;
        job_counts.nodes_drained = drained.max(0) as u64;
        // terminal outcome counters: done/failed/cancelled. A retried job that finished is counted
        // per job state, not per attempt; a `failed` state means the final attempt failed, etc.
        let done: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state=?1",
                params![state_str(JobState::Done)],
                |r| r.get::<_, i64>(0),
            )
            .map_err(internal)?;
        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state=?1",
                params![state_str(JobState::Failed)],
                |r| r.get::<_, i64>(0),
            )
            .map_err(internal)?;
        let cancelled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state=?1",
                params![state_str(JobState::Cancelled)],
                |r| r.get::<_, i64>(0),
            )
            .map_err(internal)?;
        job_counts.completed_done = done.max(0) as u64;
        job_counts.completed_failed = failed.max(0) as u64;
        job_counts.completed_cancelled = cancelled.max(0) as u64;
        job_counts.oldest_pending_at = conn
            .query_row(
                "SELECT MIN(created_at) FROM jobs WHERE state IN ('queued','blocked')",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map_err(internal)?;
        Ok(job_counts)
    }
}

type JobTuple = (
    String,
    String,
    String,
    u32,
    Option<String>,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i32>,
    String,
    i64,
    i64,
);
fn tuple_to_job(t: JobTuple) -> Result<JobRecord, StoreError> {
    let (
        id_s,
        state_s,
        spec_json,
        attempt,
        node_s,
        epoch_i,
        created_at,
        updated_at,
        started_at,
        finished_at,
        failure_s,
        summary,
        exit_code,
        artifacts_json,
        cancel_i,
        log_len_i,
    ) = t;
    Ok(JobRecord {
        id: JobId::parse(&id_s).map_err(internal)?,
        state: parse_state(&state_s)?,
        spec: decode_spec(&spec_json)?,
        attempt,
        node: match node_s {
            Some(n) => Some(NodeName::parse(&n).map_err(internal)?),
            None => None,
        },
        epoch: epoch_i as u64,
        created_at,
        updated_at,
        started_at,
        finished_at,
        failure_class: match failure_s {
            Some(f) => Some(parse_failure(&f)?),
            None => None,
        },
        summary,
        exit_code,
        artifacts: decode_artifacts(&artifacts_json)?,
        cancel_requested: cancel_i != 0,
        log_len: log_len_i as u64,
        waiting_on: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::SqliteStore;
    use crate::{Now, Store};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn temp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lu-store-test-{}-{}",
            std::process::id(),
            CTR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn conformance() {
        crate::conformance::run_all(|| {
            let p = temp_db("db.sqlite");
            SqliteStore::open(&p).unwrap()
        });
    }

    #[test]
    fn crash_safety_persist_and_reopen() {
        let p = temp_db("persist.sqlite");
        let spec = lu_proto::JobSpec {
            repo: "https://example.net/r.git".to_owned(),
            git_ref: "main".to_owned(),
            task: "t".to_owned(),
            harness: "opencode".to_owned(),
            provider: None,
            model: None,
            timeout_secs: 3600,
            idle_timeout_secs: None,
            idempotency_key: Some("k".to_owned()),
            after: Vec::new(),
        };
        let now = Now {
            wall_unix: 1,
            mono_ms: 1,
        };
        let id;
        {
            let s = SqliteStore::open(&p).unwrap();
            let (rec, created) = s.create_job(&spec, now).unwrap();
            assert!(created);
            id = rec.id;
        }
        {
            let s = SqliteStore::open(&p).unwrap();
            let rec = s.get_job(&id).unwrap();
            assert_eq!(rec.spec.repo, "https://example.net/r.git");
            assert_eq!(rec.id, id);
            // idempotency persists
            let (rec2, created2) = s.create_job(&spec, now).unwrap();
            assert!(!created2);
            assert_eq!(rec2.id, id);
        }
    }

    #[test]
    fn dependencies_survive_a_reopen() {
        let p = temp_db("deps.sqlite");
        let now = Now {
            wall_unix: 1,
            mono_ms: 1,
        };
        let (a, b) = {
            let s = SqliteStore::open(&p).unwrap();
            let (a, _) = s
                .create_job(&crate::conformance::spec("https://x/a.git", None), now)
                .unwrap();
            let mut sp = crate::conformance::spec("https://x/b.git", None);
            sp.after = vec![a.id.clone()];
            let (b, _) = s.create_job(&sp, now).unwrap();
            assert_eq!(b.state, lu_proto::JobState::Blocked);
            (a, b)
        };
        let s = SqliteStore::open(&p).unwrap();
        assert_eq!(s.recover_on_boot(now).unwrap(), 0);
        let b2 = s.get_job(&b.id).unwrap();
        assert_eq!(b2.state, lu_proto::JobState::Blocked);
        assert_eq!(b2.waiting_on, vec![a.id.clone()]);
        assert_eq!(
            b2.spec.after,
            vec![a.id],
            "dependencies are stored, not only in the spec"
        );
    }

    #[test]
    fn refuses_newer_schema_version() {
        let p = temp_db("newer.sqlite");
        {
            let _s = SqliteStore::open(&p).unwrap();
        }
        // bump version
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute("UPDATE schema_version SET version = 999", [])
            .unwrap();
        drop(conn);
        let err = SqliteStore::open(&p).unwrap_err();
        match err {
            crate::StoreError::Internal(msg) => assert!(msg.contains("newer"), "{msg}"),
            other => panic!("expected internal newer-version error, got {other:?}"),
        }
    }

    #[test]
    fn v2_database_upgrades_without_changing_old_cordons() {
        use lu_proto::{HarnessState, ProviderState};

        let p = temp_db("v2.sqlite");
        {
            let mut conn = rusqlite::Connection::open(&p).unwrap();
            conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL);")
                .unwrap();
            let tx = conn.transaction().unwrap();
            super::apply_v1(&tx).unwrap();
            super::apply_v2(&tx).unwrap();
            tx.execute("INSERT INTO schema_version(version) VALUES (2)", [])
                .unwrap();
            tx.execute(
                "INSERT INTO cordons(node,provider,until_mono_ms) VALUES ('node-a','p1',9000)",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let s = SqliteStore::open(&p).unwrap();
            let hb = crate::conformance::hb(
                "node-a",
                HarnessState::Idle,
                vec![("p1", ProviderState::Ok)],
            );
            s.heartbeat(&hb, crate::conformance::now(1, 1)).unwrap();
            s.heartbeat(&hb, crate::conformance::now(2, 2)).unwrap();
            let nodes = s.nodes(crate::conformance::now(3, 3), 60_000).unwrap();
            assert_eq!(nodes[0].cordons, vec!["p1"]);
        }
        let conn = rusqlite::Connection::open(&p).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::SCHEMA_VERSION);
        let (derived, streak): (i64, i64) = conn
            .query_row(
                "SELECT reiki_derived, healthy_hb_count FROM cordons WHERE node='node-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((derived, streak), (0, 0));
    }

    #[test]
    fn missing_directory_is_error_not_panic() {
        let p = std::env::temp_dir()
            .join(format!(
                "lu-store-missing-{}-{}",
                std::process::id(),
                CTR.fetch_add(1, Ordering::Relaxed)
            ))
            .join("sub")
            .join("db.sqlite");
        // parent does not exist
        let err = SqliteStore::open(&p).unwrap_err();
        match err {
            crate::StoreError::Internal(_) => {}
            other => panic!("expected internal error, got {other:?}"),
        }
    }
}
