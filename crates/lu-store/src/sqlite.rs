//! SQLite-backed [`crate::Store`].

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use lu_proto::{
    ArtifactName, CancelEffect, Completion, FailureClass, Heartbeat, JobEdit, JobId, JobSpec,
    JobState, NodeName, Outcome, MAX_COLLECT_BYTES, MAX_LOG_BYTES,
};

use crate::{
    Assignment, Cordon, CordonSource, JobCounts, JobRecord, NodeRecord, Now, ScopeLimits, Store,
    StoreError,
};

const CURRENT_SCHEMA_VERSION: i64 = crate::SCHEMA_VERSION;

/// Columns shared by the list/get queries that build a `JobRecord` from positional rows.
const JOB_SELECT_COLUMNS: &str = "id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,diff_bytes,assigned_provider,assigned_model,expect_failed,scrubbed_hits,patch_scrubbed,parent_job_id,root_job_id,depth,publish_json";

fn internal<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Internal(e.to_string())
}

fn state_str(s: JobState) -> &'static str {
    s.as_str()
}

fn parse_state(s: &str) -> Result<JobState, StoreError> {
    s.parse()
        .map_err(|_| internal(format!("unknown state {s}")))
}

fn failure_str(f: FailureClass) -> &'static str {
    f.as_str()
}

fn parse_failure(s: &str) -> Result<FailureClass, StoreError> {
    s.parse()
        .map_err(|_| internal(format!("unknown failure class {s}")))
}

fn parse_cordon(r: &rusqlite::Row<'_>) -> rusqlite::Result<Cordon> {
    let node: String = r.get(0)?;
    let provider: String = r.get(1)?;
    let reiki: i64 = r.get(2)?;
    let reason: String = r.get(3)?;
    let set_at_unix: i64 = r.get(4)?;
    let until_mono_ms: i64 = r.get(5)?;
    let node = NodeName::parse(&node).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    Ok(Cordon {
        node,
        provider,
        source: if reiki != 0 {
            CordonSource::Reiki
        } else {
            CordonSource::Failure
        },
        reason,
        set_at_unix,
        until_mono_ms: Some(until_mono_ms),
    })
}

fn parse_artifact(s: &str) -> Result<ArtifactName, StoreError> {
    s.parse()
        .map_err(|_| internal(format!("unknown artifact {s}")))
}

fn parse_job_id_opt(value: Option<String>) -> Result<Option<JobId>, StoreError> {
    value
        .map(|id| JobId::parse(&id).map_err(internal))
        .transpose()
}

fn encode_artifacts(arts: &[ArtifactName]) -> String {
    let v: Vec<&str> = arts.iter().map(|a| a.file_name()).collect();
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

fn decode_publish(value: Option<&str>) -> Result<Option<lu_proto::PublishRecord>, StoreError> {
    match value {
        None => Ok(None),
        Some(json) => serde_json::from_str(json)
            .map(Some)
            .map_err(|e| internal(format!("invalid stored publish record: {e}"))),
    }
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
    /// Concurrency caps installed by the controller at startup.
    group_limits: Mutex<HashMap<String, usize>>,
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
            group_limits: Mutex::new(HashMap::new()),
        };
        store.migrate()?;
        store.ensure_indexes()?;
        Ok(store)
    }

    /// Priority and group live in the existing JSON spec, so these expression indexes keep
    /// dispatch ordering and active-group counts off a full scan. Re-running on every open makes
    /// them present in databases created before this feature without a schema bump.
    fn ensure_indexes(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        conn.execute_batch(
            "DROP INDEX IF EXISTS jobs_dispatch_order;
             CREATE INDEX jobs_dispatch_order
                 ON jobs(COALESCE(json_extract(spec_json, '$.priority'), 5) DESC,
                         dispatch_seq ASC)
                 WHERE state='queued';
             CREATE INDEX IF NOT EXISTS jobs_group_active
                 ON jobs(json_extract(spec_json, '$.group'))
                 WHERE state IN ('leased','running','recovering','needs_input');
             CREATE INDEX IF NOT EXISTS jobs_parent_lineage
                 ON jobs(parent_job_id)
                 WHERE parent_job_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS jobs_root_lineage
                 ON jobs(root_job_id)
                 WHERE root_job_id IS NOT NULL;",
        )
        .map_err(internal)?;
        Ok(())
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
            Some(v) if v == CURRENT_SCHEMA_VERSION => {
                tx.commit().map_err(internal)?;
                Ok(())
            }
            // Schemas 1 and 2 add additive job columns. Fill them with zero/false so older jobs
            // keep their history and views stay compatible. Each column is checked because a
            // database briefly opened by a newer binary may already have some of them.
            Some(1) | Some(2) => {
                for (name, ddl) in [
                    (
                        "scrubbed_hits",
                        "ALTER TABLE jobs ADD COLUMN scrubbed_hits INTEGER NOT NULL DEFAULT 0",
                    ),
                    (
                        "patch_scrubbed",
                        "ALTER TABLE jobs ADD COLUMN patch_scrubbed INTEGER NOT NULL DEFAULT 0",
                    ),
                    (
                        "parent_job_id",
                        "ALTER TABLE jobs ADD COLUMN parent_job_id TEXT",
                    ),
                    ("root_job_id", "ALTER TABLE jobs ADD COLUMN root_job_id TEXT"),
                    (
                        "depth",
                        "ALTER TABLE jobs ADD COLUMN depth INTEGER NOT NULL DEFAULT 0",
                    ),
                    (
                        "publish_json",
                        "ALTER TABLE jobs ADD COLUMN publish_json TEXT",
                    ),
                ] {
                    if !job_column_exists(&tx, name)? {
                        tx.execute(ddl, []).map_err(internal)?;
                    }
                }
                tx.execute("UPDATE schema_version SET version = 3", [])
                    .map_err(internal)?;
                tx.commit().map_err(internal)?;
                Ok(())
            }
            Some(v) => Err(internal(format!(
                "database schema version {v} is not supported; create a new database (this binary supports schema version {CURRENT_SCHEMA_VERSION})"
            ))),
            None => {
                apply_baseline(&tx)?;
                tx.execute(
                    "INSERT INTO schema_version(version) VALUES (?1)",
                    params![CURRENT_SCHEMA_VERSION],
                )
                .map_err(internal)?;
                tx.commit().map_err(internal)?;
                Ok(())
            }
        }
    }
}

/// True when `jobs` already has `name`. Used by the v1 migration so a partially upgraded database
/// does not fail with `duplicate column name`.
fn job_column_exists(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<bool, StoreError> {
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .map_err(internal)?;
    Ok(count > 0)
}

fn apply_baseline(tx: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    tx.execute_batch(
        "CREATE TABLE jobs (
            id TEXT PRIMARY KEY,
            dispatch_seq INTEGER NOT NULL,
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
            recovering_since_mono_ms INTEGER,
            assigned_provider TEXT,
            assigned_model TEXT,
            diff_bytes INTEGER,
            expect_failed INTEGER NOT NULL DEFAULT 0,
            scrubbed_hits INTEGER NOT NULL DEFAULT 0,
            patch_scrubbed INTEGER NOT NULL DEFAULT 0,
            parent_job_id TEXT,
            root_job_id TEXT,
            depth INTEGER NOT NULL DEFAULT 0,
            publish_json TEXT
        );
        CREATE TABLE nodes (
            name TEXT PRIMARY KEY,
            heartbeat_json TEXT NOT NULL,
            last_seen_wall INTEGER NOT NULL,
            last_seen_mono_ms INTEGER NOT NULL,
            draining INTEGER NOT NULL
        );
        CREATE TABLE cordons (
            node TEXT NOT NULL,
            provider TEXT NOT NULL,
            until_mono_ms INTEGER NOT NULL,
            reiki_derived INTEGER NOT NULL DEFAULT 0,
            healthy_hb_count INTEGER NOT NULL DEFAULT 0,
            reason TEXT NOT NULL DEFAULT '',
            set_at_unix INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(node, provider)
        );
        CREATE TABLE idempotency (
            key TEXT PRIMARY KEY,
            spec_hash TEXT NOT NULL,
            job_id TEXT NOT NULL
        );
        CREATE TABLE job_deps (
            job TEXT NOT NULL,
            dep TEXT NOT NULL,
            ord INTEGER NOT NULL,
            PRIMARY KEY(job, dep),
            FOREIGN KEY(job) REFERENCES jobs(id),
            FOREIGN KEY(dep) REFERENCES jobs(id)
        );
        CREATE INDEX job_deps_dep ON job_deps(dep);
        CREATE INDEX jobs_idempotency_key
            ON jobs(json_extract(spec_json, '$.idempotency_key'));",
    )
    .map_err(internal)?;
    Ok(())
}

fn cordon_reason(provider: &str, completion: &Completion, source: CordonSource) -> String {
    let detail = completion
        .summary
        .as_deref()
        .unwrap_or("no failure summary")
        .trim();
    let detail = if detail.is_empty() {
        "no failure summary"
    } else {
        detail
    };
    match source {
        CordonSource::Reiki => {
            let cooldown = completion
                .retry_after_secs
                .map(|s| format!(" (retry after {s}s)"))
                .unwrap_or_default();
            format!("Reiki cooldown for provider {provider}{cooldown}: {detail}")
        }
        CordonSource::Failure => format!("Provider {provider} failed: {detail}"),
    }
}

fn load_waiting(conn: &Connection, id: &str) -> Result<Vec<JobId>, StoreError> {
    let spec_json: String = conn
        .query_row("SELECT spec_json FROM jobs WHERE id=?1", params![id], |r| {
            r.get(0)
        })
        .map_err(internal)?;
    let spec: JobSpec = serde_json::from_str(&spec_json).map_err(internal)?;
    let mut stmt = conn
        .prepare(
            "SELECT d.dep,j.state FROM job_deps d JOIN jobs j ON j.id=d.dep
             WHERE d.job=?1 ORDER BY d.ord",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(internal)?;
    let mut out = Vec::new();
    for row in rows {
        let (dep, state) = row.map_err(internal)?;
        let satisfied = state == "done"
            || (spec.include_failed && matches!(state.as_str(), "failed" | "cancelled"));
        if !satisfied {
            out.push(JobId::parse(&dep).map_err(internal)?);
        }
    }
    Ok(out)
}

fn blocked_dependents(
    tx: &rusqlite::Transaction<'_>,
    dep: &str,
) -> Result<Vec<(String, JobSpec)>, StoreError> {
    let mut stmt = tx
        .prepare(
            "SELECT j.id,j.spec_json FROM jobs j JOIN job_deps d ON d.job=j.id
             WHERE d.dep=?1 AND j.state='blocked'",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![dep], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(internal)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, json) = row.map_err(internal)?;
        let spec: JobSpec = serde_json::from_str(&json).map_err(internal)?;
        out.push((id, spec));
    }
    Ok(out)
}

fn deps_satisfied(
    tx: &rusqlite::Transaction<'_>,
    job: &str,
    include_failed: bool,
) -> Result<bool, StoreError> {
    let mut stmt = tx
        .prepare(
            "SELECT j.state FROM job_deps d JOIN jobs j ON j.id=d.dep
             WHERE d.job=?1 ORDER BY d.ord",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![job], |r| r.get::<_, String>(0))
        .map_err(internal)?;
    for row in rows {
        match row.map_err(internal)?.as_str() {
            "done" => {}
            "failed" | "cancelled" if include_failed => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Move one blocked job to the queue if every dependency it is waiting on is now acceptable.
/// `include_failed` comes from the job's spec: failed and cancelled dependencies count as
/// satisfied only when the job opted into collecting them as inputs.
fn release_blocked_job(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    include_failed: bool,
    now: Now,
) -> Result<(), StoreError> {
    if deps_satisfied(tx, id, include_failed)? {
        tx.execute(
            "UPDATE jobs SET state='queued',updated_at=?2,failure_class=NULL,
             summary=NULL,finished_at=NULL WHERE id=?1 AND state='blocked'",
            params![id, now.wall_unix],
        )
        .map_err(internal)?;
    }
    Ok(())
}

fn release_dependents(
    tx: &rusqlite::Transaction<'_>,
    dep: &str,
    now: Now,
) -> Result<(), StoreError> {
    for (id, spec) in blocked_dependents(tx, dep)? {
        release_blocked_job(tx, &id, spec.include_failed, now)?;
    }
    Ok(())
}

fn fail_dependents(
    tx: &rusqlite::Transaction<'_>,
    dep: &str,
    dep_state: &str,
    now: Now,
) -> Result<(), StoreError> {
    let mut queue = vec![(dep.to_owned(), dep_state.to_owned())];
    while let Some((dep_id, dep_state)) = queue.pop() {
        for (id, spec) in blocked_dependents(tx, &dep_id)? {
            // Opting into failed inputs means the dependency is data, not a reason to fail.
            if spec.include_failed {
                release_blocked_job(tx, &id, spec.include_failed, now)?;
                continue;
            }
            let summary = format!("dependency {dep_id} {dep_state}");
            tx.execute(
                "UPDATE jobs SET state='failed',failure_class='dependency',summary=?2,
                 finished_at=?3,updated_at=?3,node=NULL,lease_until_mono_ms=NULL,
                 recovering_since_mono_ms=NULL,cancel_requested=0 WHERE id=?1",
                params![id, summary, now.wall_unix],
            )
            .map_err(internal)?;
            queue.push((id, JobState::Failed.as_str().to_owned()));
        }
    }
    Ok(())
}

/// True when following `job -> dep` edges from `start` can reach `target`. Used to reject an
/// edited dependency list that would make a job depend on itself.
fn depends_on(
    tx: &rusqlite::Transaction<'_>,
    start: &str,
    target: &str,
) -> Result<bool, StoreError> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![start.to_owned()];
    while let Some(id) = stack.pop() {
        if id == target {
            return Ok(true);
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut stmt = tx
            .prepare("SELECT dep FROM job_deps WHERE job=?1")
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![id], |r| r.get::<_, String>(0))
            .map_err(internal)?;
        for row in rows {
            stack.push(row.map_err(internal)?);
        }
    }
    Ok(false)
}

fn get_job_tx(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
) -> Result<(JobRecord, Option<u64>, Option<u64>), StoreError> {
    let mut stmt = tx
        .prepare("SELECT id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,diff_bytes,lease_until_mono_ms,recovering_since_mono_ms,assigned_provider,assigned_model,expect_failed,scrubbed_hits,patch_scrubbed,parent_job_id,root_job_id,depth,publish_json FROM jobs WHERE id=?1")
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
                r.get::<_, Option<i64>>("diff_bytes")?,
                r.get::<_, Option<i64>>("lease_until_mono_ms")?,
                r.get::<_, Option<i64>>("recovering_since_mono_ms")?,
                r.get::<_, Option<String>>("assigned_provider")?,
                r.get::<_, Option<String>>("assigned_model")?,
                r.get::<_, i64>("expect_failed")?,
                r.get::<_, i64>("scrubbed_hits")?,
                r.get::<_, i64>("patch_scrubbed")?,
                r.get::<_, Option<String>>("parent_job_id")?,
                r.get::<_, Option<String>>("root_job_id")?,
                r.get::<_, i64>("depth")?,
                r.get::<_, Option<String>>("publish_json")?,
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
        diff_bytes,
        lease_until,
        recovering_since,
        assigned_provider,
        assigned_model,
        expect_failed,
        scrubbed_hits,
        patch_scrubbed,
        parent_job_id,
        root_job_id,
        depth,
        publish_json,
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
        assigned_provider,
        assigned_model,
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
        diff_bytes: diff_bytes.map(|v| v as u64),
        expect_failed: expect_failed != 0,
        scrubbed_hits: scrubbed_hits as u64,
        patch_scrubbed: patch_scrubbed != 0,
        waiting_on: load_waiting(tx, &id_s)?,
        parent_job_id: parse_job_id_opt(parent_job_id)?,
        root_job_id: parse_job_id_opt(root_job_id)?,
        depth: depth.max(0) as u32,
        publish: decode_publish(publish_json.as_deref())?,
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
    let publish_json = rec
        .publish
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(internal)?;
    tx.execute(
        "UPDATE jobs SET state=?2,spec_json=?3,attempt=?4,node=?5,epoch=?6,created_at=?7,updated_at=?8,started_at=?9,finished_at=?10,failure_class=?11,summary=?12,exit_code=?13,artifacts_json=?14,cancel_requested=?15,log_len=?16,lease_until_mono_ms=?17,recovering_since_mono_ms=?18,assigned_provider=?19,assigned_model=?20,diff_bytes=?21,expect_failed=?22,scrubbed_hits=?23,patch_scrubbed=?24,parent_job_id=?25,root_job_id=?26,depth=?27,publish_json=?28 WHERE id=?1",
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
            rec.assigned_provider,
            rec.assigned_model,
            rec.diff_bytes.map(|v| v as i64),
            if rec.expect_failed { 1i64 } else { 0i64 },
            rec.scrubbed_hits as i64,
            if rec.patch_scrubbed { 1i64 } else { 0i64 },
            rec.parent_job_id.as_ref().map(|id| id.as_str()),
            rec.root_job_id.as_ref().map(|id| id.as_str()),
            rec.depth as i64,
            publish_json,
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

impl SqliteStore {
    /// Creates a job, optionally with lineage. Counts are checked inside the same write
    /// transaction so two concurrent submissions cannot both slip past a cap.
    fn create_job_inner(
        &self,
        spec: &JobSpec,
        now: Now,
        lineage: Option<(&JobId, &JobId, u32)>,
        max_children: u64,
        max_descendants: u64,
    ) -> Result<(JobRecord, bool), StoreError> {
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
        if let Some((parent, root, _)) = lineage {
            let parent_state: Option<String> = tx
                .query_row(
                    "SELECT state FROM jobs WHERE id=?1",
                    params![parent.as_str()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(internal)?;
            if parent_state.is_none() {
                return Err(StoreError::NotFound);
            }
            let children: u64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM jobs WHERE parent_job_id=?1",
                    params![parent.as_str()],
                    |r| r.get::<_, i64>(0),
                )
                .map(|count| count.max(0) as u64)
                .map_err(internal)?;
            if children >= max_children {
                return Err(StoreError::LimitExceeded("max_children_per_job"));
            }
            let descendants: u64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM jobs WHERE root_job_id=?1",
                    params![root.as_str()],
                    |r| r.get::<_, i64>(0),
                )
                // The root row carries its own id, so it is not a descendant of itself.
                .map(|count| (count - 1).max(0) as u64)
                .map_err(internal)?;
            if descendants >= max_descendants {
                return Err(StoreError::LimitExceeded("max_descendants_per_root"));
            }
        }
        let id = JobId::new();
        // Every job is the root of its own spawn tree until a controller explicitly parents it.
        // Keeping the root id on top-level jobs makes the lineage self-describing and lets the
        // descendant count below subtract exactly one row.
        let root_job_id = lineage
            .map(|(_, root, _)| root.clone())
            .unwrap_or_else(|| id.clone());
        // BEGIN IMMEDIATE serialises writers, so max+1 is a stable submission sequence even when
        // several controllers' requests create jobs concurrently.
        let dispatch_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(dispatch_seq), 0) + 1 FROM jobs",
                [],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let mut blocked = false;
        let collects_files = spec.collect.collects_files();
        let mut collected_bytes = 0u64;
        for dep in &spec.after {
            let (dep_state, diff_bytes): (Option<String>, Option<i64>) = tx
                .query_row(
                    "SELECT state, diff_bytes FROM jobs WHERE id=?1",
                    params![dep.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(internal)?
                .unwrap_or((None, None));
            match dep_state.as_deref() {
                Some("done") => {}
                Some("failed" | "cancelled") if spec.include_failed => {}
                Some("failed" | "cancelled") => {
                    return Err(StoreError::DeadDependency(dep.clone()))
                }
                Some(_) => blocked = true,
                None => return Err(StoreError::InvalidDependency(dep.clone())),
            }
            if collects_files {
                let bytes = diff_bytes.unwrap_or(0).max(0) as u64;
                collected_bytes = collected_bytes.saturating_add(bytes);
                if collected_bytes > MAX_COLLECT_BYTES {
                    return Err(StoreError::InvalidSpec(format!(
                        "collect: input diffs are {collected_bytes} bytes, over the {MAX_COLLECT_BYTES}-byte cap"
                    )));
                }
            }
        }
        let mut rec = JobRecord {
            id: id.clone(),
            state: if spec.draft {
                JobState::Draft
            } else if blocked {
                JobState::Blocked
            } else {
                JobState::Queued
            },
            spec: spec.clone(),
            attempt: 1,
            node: None,
            assigned_provider: None,
            assigned_model: None,
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
            diff_bytes: None,
            expect_failed: false,
            scrubbed_hits: 0,
            patch_scrubbed: false,
            waiting_on: Vec::new(),
            parent_job_id: lineage.map(|(parent, _, _)| parent.clone()),
            root_job_id: Some(root_job_id.clone()),
            depth: lineage.map(|(_, _, depth)| depth).unwrap_or(0),
            publish: None,
        };
        tx.execute(
            "INSERT INTO jobs(id,dispatch_seq,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,lease_until_mono_ms,recovering_since_mono_ms,parent_job_id,root_job_id,depth)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                rec.id.as_str(), dispatch_seq, state_str(rec.state), canonical, 1i64, Option::<String>::None, 0i64,
                rec.created_at, rec.updated_at, Option::<i64>::None, Option::<i64>::None,
                Option::<String>::None, Option::<String>::None, Option::<i32>::None,
                encode_artifacts(&[]), 0i64, 0i64, Option::<i64>::None, Option::<i64>::None,
                rec.parent_job_id.as_ref().map(|id| id.as_str()),
                rec.root_job_id.as_ref().map(|id| id.as_str()),
                rec.depth as i64,
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
}

impl Store for SqliteStore {
    fn create_job(&self, spec: &JobSpec, now: Now) -> Result<(JobRecord, bool), StoreError> {
        self.create_job_inner(spec, now, None, 0, 0)
    }

    fn create_scoped_job(
        &self,
        spec: &JobSpec,
        now: Now,
        parent_job_id: &JobId,
        root_job_id: &JobId,
        depth: u32,
        limits: ScopeLimits,
    ) -> Result<(JobRecord, bool), StoreError> {
        self.create_job_inner(
            spec,
            now,
            Some((parent_job_id, root_job_id, depth)),
            limits.max_children,
            limits.max_descendants,
        )
    }

    fn children(&self, parent_job_id: &JobId) -> Result<Vec<JobRecord>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {JOB_SELECT_COLUMNS} FROM jobs WHERE parent_job_id=?1 ORDER BY dispatch_seq DESC"
            ))
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![parent_job_id.as_str()], |row| {
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
                    row.get::<_, Option<i64>>(16)?,
                    row.get::<_, Option<String>>(17)?,
                    row.get::<_, Option<String>>(18)?,
                    row.get::<_, i64>(19)?,
                    row.get::<_, i64>(20)?,
                    row.get::<_, i64>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    row.get::<_, Option<String>>(23)?,
                    row.get::<_, i64>(24)?,
                    row.get::<_, Option<String>>(25)?,
                ))
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(tuple_to_job(row.map_err(internal)?)?);
        }
        Ok(out)
    }

    fn descendants(&self, root_job_id: &JobId) -> Result<u64, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE root_job_id=?1",
                params![root_job_id.as_str()],
                |r| r.get(0),
            )
            .map_err(internal)?;
        Ok((count - 1).max(0) as u64)
    }

    fn get_job(&self, id: &JobId) -> Result<JobRecord, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {JOB_SELECT_COLUMNS} FROM jobs WHERE id=?1"
            ))
            .map_err(internal)?;
        let t: JobTuple = stmt
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
                    row.get::<_, Option<i64>>(16)?,
                    row.get::<_, Option<String>>(17)?,
                    row.get::<_, Option<String>>(18)?,
                    row.get::<_, i64>(19)?,
                    row.get::<_, i64>(20)?,
                    row.get::<_, i64>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    row.get::<_, Option<String>>(23)?,
                    row.get::<_, i64>(24)?,
                    row.get::<_, Option<String>>(25)?,
                ))
            })
            .optional()
            .map_err(internal)?
            .ok_or(StoreError::NotFound)?;
        let mut rec = tuple_to_job(t)?;
        rec.waiting_on = load_waiting(&conn, rec.id.as_str())?;
        Ok(rec)
    }

    fn leased_job(
        &self,
        id: &JobId,
        node: &NodeName,
        epoch: u64,
        now: Now,
        lease_ms: u64,
    ) -> Result<JobRecord, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {JOB_SELECT_COLUMNS}, lease_until_mono_ms FROM jobs
                 WHERE id=?1 AND node=?2 AND epoch=?3
                   AND state IN ('leased','running','needs_input')
                   AND lease_until_mono_ms > ?4"
            ))
            .map_err(internal)?;
        let row: Option<(JobTuple, i64)> = stmt
            .query_row(
                params![id.as_str(), node.as_str(), epoch as i64, now.mono_ms as i64],
                |row| {
                    let t: JobTuple = (
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                        row.get(16)?,
                        row.get(17)?,
                        row.get(18)?,
                        row.get(19)?,
                        row.get(20)?,
                        row.get(21)?,
                        row.get(22)?,
                        row.get(23)?,
                        row.get(24)?,
                        row.get(25)?,
                    );
                    Ok((t, row.get(26)?))
                },
            )
            .optional()
            .map_err(internal)?;
        let _ = lease_ms;
        let (t, _) = row.ok_or(StoreError::StaleEpoch)?;
        let mut rec = tuple_to_job(t)?;
        rec.waiting_on = load_waiting(&conn, rec.id.as_str())?;
        Ok(rec)
    }

    fn job_by_key(&self, key: &str) -> Option<JobRecord> {
        let conn = self.conn.lock().ok()?;
        let t: JobTuple = conn
            .query_row(
                &format!(
                    "SELECT {JOB_SELECT_COLUMNS} FROM jobs
                 WHERE json_extract(spec_json, '$.idempotency_key') = ?1
                 ORDER BY id DESC LIMIT 1"
                ),
                params![key],
                |row| {
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
                        row.get::<_, Option<i64>>(16)?,
                        row.get::<_, Option<String>>(17)?,
                        row.get::<_, Option<String>>(18)?,
                        row.get::<_, i64>(19)?,
                        row.get::<_, i64>(20)?,
                        row.get::<_, i64>(21)?,
                        row.get::<_, Option<String>>(22)?,
                        row.get::<_, Option<String>>(23)?,
                        row.get::<_, i64>(24)?,
                        row.get::<_, Option<String>>(25)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        let mut rec = tuple_to_job(t).ok()?;
        rec.waiting_on = load_waiting(&conn, rec.id.as_str()).ok()?;
        Some(rec)
    }

    fn list_jobs(
        &self,
        state: Option<JobState>,
        limit: usize,
        after: Option<&JobId>,
    ) -> Result<Vec<JobRecord>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut out = Vec::new();
        let base = format!("SELECT {JOB_SELECT_COLUMNS} FROM jobs");
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
                            row.get::<_, Option<i64>>(16)?,
                            row.get::<_, Option<String>>(17)?,
                            row.get::<_, Option<String>>(18)?,
                            row.get::<_, i64>(19)?,
                            row.get::<_, i64>(20)?,
                            row.get::<_, i64>(21)?,
                            row.get::<_, Option<String>>(22)?,
                            row.get::<_, Option<String>>(23)?,
                            row.get::<_, i64>(24)?,
                            row.get::<_, Option<String>>(25)?,
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
                            row.get::<_, Option<i64>>(16)?,
                            row.get::<_, Option<String>>(17)?,
                            row.get::<_, Option<String>>(18)?,
                            row.get::<_, i64>(19)?,
                            row.get::<_, i64>(20)?,
                            row.get::<_, i64>(21)?,
                            row.get::<_, Option<String>>(22)?,
                            row.get::<_, Option<String>>(23)?,
                            row.get::<_, i64>(24)?,
                            row.get::<_, Option<String>>(25)?,
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
                            row.get::<_, Option<i64>>(16)?,
                            row.get::<_, Option<String>>(17)?,
                            row.get::<_, Option<String>>(18)?,
                            row.get::<_, i64>(19)?,
                            row.get::<_, i64>(20)?,
                            row.get::<_, i64>(21)?,
                            row.get::<_, Option<String>>(22)?,
                            row.get::<_, Option<String>>(23)?,
                            row.get::<_, i64>(24)?,
                            row.get::<_, Option<String>>(25)?,
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
                            row.get::<_, Option<i64>>(16)?,
                            row.get::<_, Option<String>>(17)?,
                            row.get::<_, Option<String>>(18)?,
                            row.get::<_, i64>(19)?,
                            row.get::<_, i64>(20)?,
                            row.get::<_, i64>(21)?,
                            row.get::<_, Option<String>>(22)?,
                            row.get::<_, Option<String>>(23)?,
                            row.get::<_, i64>(24)?,
                            row.get::<_, Option<String>>(25)?,
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
                fail_dependents(&tx, rec.id.as_str(), JobState::Cancelled.as_str(), now)?;
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

    fn edit_job(&self, id: &JobId, edit: &JobEdit, now: Now) -> Result<JobRecord, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        if !matches!(
            rec.state,
            JobState::Draft | JobState::Queued | JobState::Blocked
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        rec.spec = edit
            .apply(&rec.spec)
            .map_err(|e| StoreError::InvalidSpec(e.to_string()))?;
        // A future attempt uses the new provider choice, so drop the old assignment.
        rec.assigned_provider = None;
        rec.assigned_model = None;
        rec.updated_at = now.wall_unix;

        if edit.after.is_some() {
            tx.execute("DELETE FROM job_deps WHERE job=?1", params![id.as_str()])
                .map_err(internal)?;
            let mut blocked = false;
            for dep in &rec.spec.after {
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
                    Some("failed" | "cancelled") if rec.spec.include_failed => {}
                    Some("failed" | "cancelled") => {
                        return Err(StoreError::DeadDependency(dep.clone()))
                    }
                    Some(_) => blocked = true,
                    None => return Err(StoreError::InvalidDependency(dep.clone())),
                }
            }
            for dep in &rec.spec.after {
                if depends_on(&tx, dep.as_str(), id.as_str())? {
                    return Err(StoreError::DependencyCycle);
                }
            }
            for (ord, dep) in rec.spec.after.iter().enumerate() {
                tx.execute(
                    "INSERT INTO job_deps(job,dep,ord) VALUES (?1,?2,?3)",
                    params![id.as_str(), dep.as_str(), ord as i64],
                )
                .map_err(internal)?;
            }
            // Editing a draft never dispatches it: only an explicit approval changes the state.
            if rec.state != JobState::Draft {
                rec.state = if blocked {
                    JobState::Blocked
                } else {
                    JobState::Queued
                };
            }
        }
        rec.waiting_on = load_waiting(&tx, id.as_str())?;
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn list_drafts(&self, limit: usize) -> Result<Vec<JobRecord>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {JOB_SELECT_COLUMNS} FROM jobs WHERE state='draft' ORDER BY created_at ASC, id ASC LIMIT ?1"
            ))
            .map_err(internal)?;
        let rows = stmt
            .query_map(
                params![limit.min(i64::MAX as usize) as i64],
                row_to_job_tuple,
            )
            .map_err(internal)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(tuple_to_job(row.map_err(internal)?)?);
        }
        Ok(out)
    }

    fn draft_counts(&self) -> Result<crate::DraftCounts, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM jobs WHERE state='draft'", [], |r| {
                r.get(0)
            })
            .map_err(internal)?;
        let oldest: Option<i64> = conn
            .query_row(
                "SELECT MIN(created_at) FROM jobs WHERE state='draft'",
                [],
                |r| r.get(0),
            )
            .map_err(internal)?;
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(json_extract(spec_json, '$.origin'), ''), COUNT(*) \
                 FROM jobs WHERE state='draft' GROUP BY 1 ORDER BY 1",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(internal)?;
        let mut by_origin = Vec::new();
        for row in rows {
            let (origin, count) = row.map_err(internal)?;
            by_origin.push((origin, count.max(0) as u64));
        }
        Ok(crate::DraftCounts {
            total: total.max(0) as u64,
            by_origin,
            oldest_created_at: oldest,
        })
    }

    fn approve_draft(
        &self,
        id: &JobId,
        target: JobState,
        now: Now,
    ) -> Result<JobRecord, StoreError> {
        if !matches!(target, JobState::Blocked | JobState::Queued) {
            return Err(StoreError::Conflict { length: None });
        }
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        if rec.state != JobState::Draft {
            return Err(StoreError::Conflict { length: None });
        }
        check_transition(rec.state, target)?;
        rec.state = target;
        rec.updated_at = now.wall_unix;
        rec.waiting_on = load_waiting(&tx, rec.id.as_str())?;
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn expire_drafts(&self, older_than: i64, now: Now) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT id FROM jobs WHERE state='draft' AND created_at < ?1")
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![older_than], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row.map_err(internal)?);
            }
            ids
        };
        for id in &ids {
            let (mut rec, lease, recov) = get_job_tx(&tx, id)?;
            check_transition(rec.state, JobState::Expired)?;
            rec.state = JobState::Expired;
            rec.updated_at = now.wall_unix;
            rec.finished_at = Some(now.wall_unix);
            update_job_tx(&tx, &rec, lease, recov)?;
        }
        tx.commit().map_err(internal)?;
        Ok(ids.len() as u64)
    }

    fn expire_oldest_drafts(&self, count: u64, now: Now) -> Result<u64, StoreError> {
        if count == 0 {
            return Ok(0);
        }
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT id FROM jobs WHERE state='draft' ORDER BY created_at, id LIMIT ?1")
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![count.min(i64::MAX as u64) as i64], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(internal)?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row.map_err(internal)?);
            }
            ids
        };
        for id in &ids {
            let (mut rec, lease, recov) = get_job_tx(&tx, id)?;
            check_transition(rec.state, JobState::Expired)?;
            rec.state = JobState::Expired;
            rec.updated_at = now.wall_unix;
            rec.finished_at = Some(now.wall_unix);
            update_job_tx(&tx, &rec, lease, recov)?;
        }
        tx.commit().map_err(internal)?;
        Ok(ids.len() as u64)
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
        // A recovering job still belongs to its node: the agent may come back for it, so the
        // node must not take a second job in the meantime.
        let held: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM jobs WHERE node=?1 AND state IN ('leased','running','recovering') LIMIT 1",
                params![node.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        if held.is_some() {
            return Ok(None);
        }
        // Group caps are counted inside this write transaction, so two concurrent polls cannot
        // both take the last slot of a group.
        let limits = self
            .group_limits
            .lock()
            .map_err(|_| internal("mutex poisoned"))?;
        let mut active: HashMap<String, usize> = {
            let mut stmt = tx
                .prepare(
                    "SELECT json_extract(spec_json, '$.group') FROM jobs
                     WHERE state IN ('leased','running','recovering','needs_input')
                       AND json_extract(spec_json, '$.group') IS NOT NULL",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut counts = HashMap::new();
            for row in rows {
                *counts.entry(row.map_err(internal)?).or_insert(0) += 1;
            }
            counts
        };
        // Higher priority first, then submission order. `dispatch_seq` is persisted so wall
        // clock steps and same-millisecond ids cannot reorder the queue. A spec from before the
        // priority field existed has no JSON value and sorts as the default priority.
        let candidates: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id FROM jobs WHERE state='queued'
                     ORDER BY COALESCE(json_extract(spec_json, '$.priority'), 5) DESC,
                              dispatch_seq ASC",
                )
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
            if let Some(group) = &rec.spec.group {
                if let Some(&limit) = limits.get(group) {
                    if active.get(group).copied().unwrap_or(0) >= limit {
                        continue;
                    }
                }
            }
            if !crate::labels_satisfied(&rec.spec.requires, &hb.labels) {
                continue;
            }
            if !crate::taints_tolerated(&hb.taints, &rec.spec.tolerates) {
                continue;
            }
            // A job that names no provider runs anywhere, as before. `provider = "any"` takes
            // the healthy pairs the agent published, in preference order. Otherwise the primary
            // is tried first, then the fallbacks; a fallback without a model keeps the primary
            // model. Every choice still needs this node to serve it.
            let choices: Vec<(String, Option<String>)> = if rec.spec.is_any_provider() {
                hb.provider_models
                    .iter()
                    .filter(|pair| node_offers_provider(&hb, &pair.provider))
                    .map(|pair| (pair.provider.clone(), Some(pair.model.clone())))
                    .collect()
            } else {
                match &rec.spec.provider {
                    Some(primary) => std::iter::once((primary.clone(), rec.spec.model.clone()))
                        .chain(rec.spec.fallbacks.iter().map(|choice| {
                            (
                                choice.provider.clone(),
                                choice.model.clone().or_else(|| rec.spec.model.clone()),
                            )
                        }))
                        .collect(),
                    None => Vec::new(),
                }
            };
            // Only a job with no provider at all may run without an assignment. An `any` job with
            // no healthy pair waits for a node that has one.
            let mut chosen = if choices.is_empty() && rec.spec.provider.is_none() {
                Some((None, None))
            } else {
                None
            };
            for (provider, model) in &choices {
                let cordoned: Option<i64> = tx
                    .query_row(
                        "SELECT until_mono_ms FROM cordons WHERE node=?1 AND provider=?2",
                        params![node.as_str(), provider],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if let Some(until) = cordoned {
                    if (until as u64) > now.mono_ms {
                        continue;
                    }
                }
                if node_offers_provider(&hb, provider) {
                    chosen = Some((Some(provider.clone()), model.clone()));
                    break;
                }
            }
            let Some((assigned_provider, assigned_model)) = chosen else {
                continue;
            };
            check_transition(rec.state, JobState::Leased)?;
            rec.state = JobState::Leased;
            rec.node = Some(node.clone());
            rec.assigned_provider = assigned_provider;
            rec.assigned_model = assigned_model;
            rec.epoch = rec.epoch.saturating_add(1);
            rec.updated_at = now.wall_unix;
            rec.failure_class = None;
            rec.summary = None;
            rec.exit_code = None;
            rec.finished_at = None;
            let lease_until = Some(now.mono_ms.saturating_add(lease_ms));
            update_job_tx(&tx, &rec, lease_until, None)?;
            if let Some(group) = &rec.spec.group {
                if limits.contains_key(group) {
                    *active.entry(group.clone()).or_insert(0) += 1;
                }
            }
            tx.commit().map_err(internal)?;
            return Ok(Some(Assignment {
                job_id: rec.id,
                epoch: rec.epoch,
                provider: rec.assigned_provider.clone(),
                model: rec.assigned_model.clone(),
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

    fn held_jobs(&self, node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, epoch FROM jobs WHERE node=?1 AND state IN ('leased','running','recovering')"
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

    fn unreserve_log(
        &self,
        id: &JobId,
        epoch: u64,
        length: u64,
        n: u64,
    ) -> Result<u64, StoreError> {
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
        let Some(restored) = length.checked_sub(n) else {
            return Err(StoreError::Conflict {
                length: Some(rec.log_len),
            });
        };
        if rec.log_len != length {
            return Err(StoreError::Conflict {
                length: Some(rec.log_len),
            });
        }
        rec.log_len = restored;
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(rec.log_len)
    }

    fn record_artifact(
        &self,
        id: &JobId,
        epoch: u64,
        name: ArtifactName,
        bytes: u64,
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
        if name == ArtifactName::DiffPatch {
            rec.diff_bytes = Some(bytes);
        }
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn record_scrub(
        &self,
        id: &JobId,
        epoch: u64,
        hits: u64,
        patch: bool,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        if rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        rec.scrubbed_hits = rec.scrubbed_hits.saturating_add(hits);
        rec.patch_scrubbed |= patch;
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn record_publish(
        &self,
        id: &JobId,
        record: &lu_proto::PublishRecord,
        now: Now,
    ) -> Result<JobRecord, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        rec.publish = Some(record.clone());
        rec.updated_at = now.wall_unix;
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(rec)
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
        rec.expect_failed = completion.expect_failed;
        match completion.outcome {
            Outcome::Done => {
                check_transition(rec.state, JobState::Done)?;
                rec.state = JobState::Done;
                rec.finished_at = Some(now.wall_unix);
                rec.updated_at = now.wall_unix;
                rec.failure_class = None;
                update_job_tx(&tx, &rec, None, None)?;
                release_dependents(&tx, rec.id.as_str(), now)?;
            }
            Outcome::Cancelled => {
                check_transition(rec.state, JobState::Cancelled)?;
                rec.state = JobState::Cancelled;
                rec.finished_at = Some(now.wall_unix);
                rec.updated_at = now.wall_unix;
                update_job_tx(&tx, &rec, None, None)?;
                fail_dependents(&tx, rec.id.as_str(), JobState::Cancelled.as_str(), now)?;
            }
            Outcome::Failed(out_class) => {
                let (class, provider) = failure.unwrap_or((out_class, None));
                rec.failure_class = Some(class);
                if class.retryable(rec.attempt, max_retries) {
                    check_transition(rec.state, JobState::Queued)?;
                    rec.state = JobState::Queued;
                    rec.attempt = rec.attempt.saturating_add(1);
                    rec.node = None;
                    rec.assigned_provider = None;
                    rec.assigned_model = None;
                    rec.cancel_requested = false;
                    rec.finished_at = None;
                    rec.updated_at = now.wall_unix;
                    update_job_tx(&tx, &rec, None, None)?;
                    if class == FailureClass::Provider {
                        if let (Some(node), Some(p)) = (node_for_cordon, provider) {
                            let until = now.mono_ms.saturating_add(cordon_ms) as i64;
                            let source = if completion.retry_after_secs.is_some() {
                                CordonSource::Reiki
                            } else {
                                CordonSource::Failure
                            };
                            let reason = cordon_reason(&p, completion, source);
                            tx.execute(
                                "INSERT INTO cordons(node,provider,until_mono_ms,reiki_derived,healthy_hb_count,reason,set_at_unix)
                                 VALUES(?1,?2,?3,?4,0,?5,?6)
                                 ON CONFLICT(node,provider) DO UPDATE SET until_mono_ms=excluded.until_mono_ms,
                                 reiki_derived=excluded.reiki_derived, healthy_hb_count=0,
                                 reason=excluded.reason, set_at_unix=excluded.set_at_unix",
                                params![
                                    node.as_str(),
                                    p,
                                    until,
                                    if source == CordonSource::Reiki { 1i64 } else { 0i64 },
                                    reason,
                                    now.wall_unix
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
                    fail_dependents(&tx, rec.id.as_str(), JobState::Failed.as_str(), now)?;
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
                rec.assigned_provider = None;
                rec.assigned_model = None;
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
                fail_dependents(&tx, rec.id.as_str(), JobState::Failed.as_str(), now)?;
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
        // Cordon expiries are on the same monotonic clock: a row from the last run would look
        // hours away against a fresh clock. Reiki health and new failures set them again.
        tx.execute("DELETE FROM cordons", []).map_err(internal)?;
        // Node liveness uses that clock too. A persisted heartbeat stamp from the previous
        // process must not make a dead node look live: -1 means "not seen this process" and
        // is treated as offline until a fresh heartbeat arrives.
        tx.execute("UPDATE nodes SET last_seen_mono_ms=-1", [])
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
            let online = last_seen_mono >= 0
                && now.mono_ms.saturating_sub(last_seen_mono as u64) <= lease_ms.saturating_mul(2);
            // current job
            let current: Option<String> = conn
                .query_row(
                    "SELECT id FROM jobs WHERE node=?1 AND state IN ('leased','running','recovering') LIMIT 1",
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

    fn cordons(&self, node: Option<&NodeName>) -> Result<Vec<Cordon>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let sql =
            "SELECT node,provider,reiki_derived,reason,set_at_unix,until_mono_ms FROM cordons";
        let mut out = Vec::new();
        if let Some(node) = node {
            let mut stmt = conn
                .prepare(&format!("{sql} WHERE node=?1 ORDER BY provider"))
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![node.as_str()], parse_cordon)
                .map_err(internal)?;
            for row in rows {
                out.push(row.map_err(internal)?);
            }
        } else {
            let mut stmt = conn
                .prepare(&format!("{sql} ORDER BY node, provider"))
                .map_err(internal)?;
            let rows = stmt.query_map([], parse_cordon).map_err(internal)?;
            for row in rows {
                out.push(row.map_err(internal)?);
            }
        }
        Ok(out)
    }

    fn set_cordon(
        &self,
        node: &NodeName,
        provider: &str,
        source: CordonSource,
        reason: &str,
        set_at_unix: i64,
        until_mono_ms: i64,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        conn.execute(
            "INSERT INTO cordons(node,provider,until_mono_ms,reiki_derived,healthy_hb_count,reason,set_at_unix)
             VALUES(?1,?2,?3,?4,0,?5,?6)
             ON CONFLICT(node,provider) DO UPDATE SET until_mono_ms=excluded.until_mono_ms,
             reiki_derived=excluded.reiki_derived, healthy_hb_count=0,
             reason=excluded.reason, set_at_unix=excluded.set_at_unix",
            params![
                node.as_str(),
                provider,
                until_mono_ms,
                if source == CordonSource::Reiki { 1i64 } else { 0i64 },
                reason,
                set_at_unix
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    fn uncordon(&self, node: &NodeName, provider: Option<&str>) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut removed: Vec<String> = {
            let (sql, params) = match provider {
                Some(p) => (
                    "SELECT provider FROM cordons WHERE node=?1 AND provider=?2",
                    vec![node.as_str(), p],
                ),
                None => (
                    "SELECT provider FROM cordons WHERE node=?1",
                    vec![node.as_str()],
                ),
            };
            let mut stmt = conn.prepare(sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params), |r| {
                    r.get::<_, String>(0)
                })
                .map_err(internal)?;
            let mut v = Vec::new();
            for row in rows {
                v.push(row.map_err(internal)?);
            }
            v
        };
        match provider {
            Some(p) => {
                conn.execute(
                    "DELETE FROM cordons WHERE node=?1 AND provider=?2",
                    params![node.as_str(), p],
                )
                .map_err(internal)?;
            }
            None => {
                conn.execute("DELETE FROM cordons WHERE node=?1", params![node.as_str()])
                    .map_err(internal)?;
            }
        }
        removed.sort();
        removed.dedup();
        Ok(removed)
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
                "SELECT COUNT(*) FROM nodes WHERE last_seen_mono_ms >= 0 AND last_seen_mono_ms + (?1 * 2) >= ?2",
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

    fn set_group_limits(&self, limits: HashMap<String, usize>) -> Result<(), StoreError> {
        let mut current = self
            .group_limits
            .lock()
            .map_err(|_| internal("mutex poisoned"))?;
        *current = limits;
        Ok(())
    }

    fn group_counts(&self) -> Result<HashMap<String, usize>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare(
                "SELECT json_extract(spec_json, '$.group') FROM jobs
                 WHERE state IN ('leased','running','recovering','needs_input')
                   AND json_extract(spec_json, '$.group') IS NOT NULL",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(internal)?;
        let mut counts = HashMap::new();
        for row in rows {
            *counts.entry(row.map_err(internal)?).or_insert(0) += 1;
        }
        Ok(counts)
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
    Option<i64>,
    Option<String>,
    Option<String>,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
);

fn row_to_job_tuple(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
        row.get(19)?,
        row.get(20)?,
        row.get(21)?,
        row.get(22)?,
        row.get(23)?,
        row.get(24)?,
        row.get(25)?,
    ))
}

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
        diff_bytes,
        assigned_provider,
        assigned_model,
        expect_failed,
        scrubbed_hits,
        patch_scrubbed,
        parent_job_id,
        root_job_id,
        depth,
        publish_json,
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
        assigned_provider,
        assigned_model,
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
        diff_bytes: diff_bytes.map(|v| v as u64),
        expect_failed: expect_failed != 0,
        scrubbed_hits: scrubbed_hits as u64,
        patch_scrubbed: patch_scrubbed != 0,
        waiting_on: Vec::new(),
        parent_job_id: parse_job_id_opt(parent_job_id)?,
        root_job_id: parse_job_id_opt(root_job_id)?,
        depth: depth.max(0) as u32,
        publish: decode_publish(publish_json.as_deref())?,
    })
}

#[cfg(test)]
mod tests {
    use super::SqliteStore;
    use crate::{Now, Store};
    use rusqlite::params;
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
    #[ignore = "long randomised model run; the normal conformance test covers a shorter run"]
    fn model_long() {
        crate::model::run_long(|| {
            let p = temp_db("model-long.sqlite");
            SqliteStore::open(&p).unwrap()
        });
    }

    #[test]
    fn crash_safety_persist_and_reopen() {
        let p = temp_db("persist.sqlite");
        let spec = lu_proto::JobSpec {
            source: lu_proto::SourceSpec {
                repo: "https://example.net/r.git".to_owned(),
                git_ref: "main".to_owned(),
                base: None,
            },
            repo: "https://example.net/r.git".to_owned(),
            git_ref: "main".to_owned(),
            base_commit: None,
            task: "t".to_owned(),
            harness: "opencode".to_owned(),
            provider: None,
            model: None,
            timeout_secs: 3600,
            idle_timeout_secs: None,
            on_stuck: None,
            stuck_after_secs: None,
            idempotency_key: Some("k".to_owned()),
            after: Vec::new(),
            collect: lu_proto::CollectMode::Apply,
            include_failed: false,
            on_conflict: Default::default(),
            priority: lu_proto::DEFAULT_PRIORITY,
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
            allow_empty: false,
            requires: Vec::new(),
            tolerates: Vec::new(),
            fallbacks: Vec::new(),
            expect: None,
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
    fn refuses_unsupported_schema_version() {
        let p = temp_db("unsupported.sqlite");
        {
            let _s = SqliteStore::open(&p).unwrap();
        }
        for version in [0i64, 4, 9, 999] {
            let conn = rusqlite::Connection::open(&p).unwrap();
            conn.execute("UPDATE schema_version SET version = ?1", params![version])
                .unwrap();
            drop(conn);
            let err = SqliteStore::open(&p).unwrap_err();
            match err {
                crate::StoreError::Internal(msg) => {
                    assert!(msg.contains(&format!("schema version {version}")), "{msg}");
                    assert!(msg.contains("create a new database"), "{msg}");
                }
                other => panic!("expected unsupported schema error, got {other:?}"),
            }
        }
    }

    #[test]
    fn schema_1_database_upgrades_with_scrub_and_lineage_columns() {
        let p = temp_db("schema1.sqlite");
        {
            let s = SqliteStore::open(&p).unwrap();
            let spec = lu_proto::JobSpec {
                source: lu_proto::SourceSpec {
                    repo: "https://example.net/r.git".to_owned(),
                    git_ref: "main".to_owned(),
                    base: None,
                },
                repo: "https://example.net/r.git".to_owned(),
                git_ref: "main".to_owned(),
                base_commit: None,
                task: "t".to_owned(),
                harness: "opencode".to_owned(),
                provider: None,
                model: None,
                timeout_secs: 3600,
                idle_timeout_secs: None,
                on_stuck: None,
                stuck_after_secs: None,
                idempotency_key: None,
                after: Vec::new(),
                collect: lu_proto::CollectMode::Apply,
                include_failed: false,
                on_conflict: Default::default(),
                priority: lu_proto::DEFAULT_PRIORITY,
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
                allow_empty: false,
                requires: Vec::new(),
                tolerates: Vec::new(),
                fallbacks: Vec::new(),
                expect: None,
            };
            s.create_job(
                &spec,
                Now {
                    wall_unix: 1,
                    mono_ms: 1,
                },
            )
            .unwrap();
        }
        // Recreate the version stamp a schema 1 database would have, and remove every column
        // introduced by schema 2. The lineage indexes must go first or SQLite refuses the drop.
        {
            let conn = rusqlite::Connection::open(&p).unwrap();
            conn.execute_batch(
                "DROP INDEX IF EXISTS jobs_parent_lineage;
                 DROP INDEX IF EXISTS jobs_root_lineage;
                 ALTER TABLE jobs DROP COLUMN scrubbed_hits;
                 ALTER TABLE jobs DROP COLUMN patch_scrubbed;
                 ALTER TABLE jobs DROP COLUMN parent_job_id;
                 ALTER TABLE jobs DROP COLUMN root_job_id;
                 ALTER TABLE jobs DROP COLUMN depth;
                 UPDATE schema_version SET version = 1;",
            )
            .unwrap();
        }
        let s = SqliteStore::open(&p).unwrap();
        let conn = rusqlite::Connection::open(&p).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let jobs = s.list_jobs(None, 10, None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].scrubbed_hits, 0);
        assert!(!jobs[0].patch_scrubbed);
        assert_eq!(jobs[0].parent_job_id, None);
        assert_eq!(jobs[0].root_job_id, None);
        assert_eq!(jobs[0].depth, 0);
        s.record_scrub(&jobs[0].id, jobs[0].epoch, 3, true).unwrap();
        let job = s.get_job(&jobs[0].id).unwrap();
        assert_eq!(job.scrubbed_hits, 3);
        assert!(job.patch_scrubbed);
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
    #[test]
    fn restart_drops_cordons_stamped_by_the_previous_clock() {
        let path = temp_db("cordons.db");
        let store = SqliteStore::open(&path).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO cordons(node,provider,until_mono_ms,reiki_derived,healthy_hb_count) \
                 VALUES('dev-ct-1','zen',70000000,0,0),('dev-ct-1','openrouter',90000000,1,0)",
                [],
            )
            .unwrap();
        }
        let now = Now {
            wall_unix: 1_700_000_000,
            mono_ms: 3_000,
        };
        store.recover_on_boot(now).unwrap();
        let conn = store.conn.lock().unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM cordons", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }
}
