//! SQLite-backed [`crate::Store`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use lu_proto::{
    normalize_repository, ArtifactName, CancelEffect, Completion, DurationSummary, FailureClass,
    Heartbeat, JobEdit, JobId, JobSpec, JobState, NodeContribution, NodeFailureReason, NodeName,
    Outcome, RepoContribution, StatsAggregate, StatsQuery, StatsReport, MAX_COLLECT_BYTES,
    MAX_LOG_BYTES, STATS_BUCKETS_SECS, STATS_BUCKET_LABELS, TOP_REPOSITORIES,
};

use crate::{
    Assignment, Cordon, CordonSource, JobCounts, JobRecord, NodeRecord, Now, RequestKeyReservation,
    ScopeLimits, Store, StoreError,
};

const CURRENT_SCHEMA_VERSION: i64 = crate::SCHEMA_VERSION;

/// Columns shared by the list/get queries that build a `JobRecord` from positional rows.
const JOB_SELECT_COLUMNS: &str = "id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,diff_bytes,assigned_provider,assigned_model,expect_failed,scrubbed_hits,patch_scrubbed,patch_secret_hits_json,parent_job_id,root_job_id,depth,publish_json,staged_commit,blocked_reason,blocked_since,eligible_since,failure_reason,excluded_nodes_json";

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

fn parse_failure_reason(s: Option<String>) -> Result<Option<NodeFailureReason>, StoreError> {
    match s {
        None => Ok(None),
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| internal(format!("unknown node failure reason {text}"))),
    }
}

fn encode_excluded_nodes(nodes: &[NodeName]) -> Result<String, StoreError> {
    let names: Vec<&str> = nodes.iter().map(NodeName::as_str).collect();
    serde_json::to_string(&names).map_err(internal)
}

fn decode_excluded_nodes(text: &str) -> Result<Vec<NodeName>, StoreError> {
    let names: Vec<String> = serde_json::from_str(text).map_err(internal)?;
    names
        .into_iter()
        .map(|name| NodeName::parse(&name).map_err(internal))
        .collect()
}

/// Adds a node to a job's exclusion list without duplicating it.
fn exclude_node(rec: &mut JobRecord, node: Option<&NodeName>) {
    if let Some(node) = node {
        if !rec.excluded_nodes.contains(node) {
            rec.excluded_nodes.push(node.clone());
        }
    }
}

/// Whether this node failure can use another attempt. A repeated node, an exhausted node budget
/// or a stored diff/cancellation all make the failure terminal.
fn node_retry_allowed(rec: &JobRecord, max_retries: u32, node_attempts: u32) -> bool {
    let cap = crate::node_attempt_cap(max_retries, node_attempts);
    if rec.attempt >= cap
        || rec.excluded_nodes.len() >= usize::try_from(cap.saturating_sub(1)).unwrap_or(usize::MAX)
    {
        return false;
    }
    if rec.artifacts.contains(&ArtifactName::DiffPatch)
        || rec.diff_bytes.is_some()
        || rec.cancel_requested
    {
        return false;
    }
    !rec.node
        .as_ref()
        .is_some_and(|node| rec.excluded_nodes.contains(node))
}

fn parse_cordon(r: &rusqlite::Row<'_>) -> rusqlite::Result<Cordon> {
    let node: String = r.get(0)?;
    let provider: String = r.get(1)?;
    let health_derived: i64 = r.get(2)?;
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
        source: if health_derived != 0 {
            CordonSource::Health
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
    let bad = |e: &dyn std::fmt::Display| internal(format!("invalid stored spec: {e}"));
    let mut value: serde_json::Value = serde_json::from_str(s).map_err(|e| bad(&e))?;
    // Rows stored before credentials were refused in repo URLs must stay readable, without the
    // credentials: redact before the typed parse, which validates.
    for path in [&["repo"][..], &["source", "repo"][..]] {
        let mut slot = Some(&mut value);
        for key in path {
            slot = slot.and_then(|v| v.get_mut(*key));
        }
        if let Some(serde_json::Value::String(repo)) = slot {
            *repo = lu_proto::redact_repo(repo).into_owned();
        }
    }
    let spec: JobSpec = serde_json::from_value(value).map_err(|e| bad(&e))?;
    spec.validate().map_err(|e| bad(&e))?;
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
            // Schemas 1 through 7 add additive job columns. Fill them with zero/false/NULL so
            // older jobs keep their history and views stay compatible. Each column is checked
            // because a database briefly opened by a newer binary may already have some of them.
            Some(1) | Some(2) | Some(3) | Some(4) | Some(5) | Some(6) | Some(7) => {
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
                        "patch_secret_hits_json",
                        "ALTER TABLE jobs ADD COLUMN patch_secret_hits_json TEXT NOT NULL DEFAULT '{\"count\":0,\"rules\":[]}'",
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
                    ("staged_commit", "ALTER TABLE jobs ADD COLUMN staged_commit TEXT"),
                    (
                        "blocked_reason",
                        "ALTER TABLE jobs ADD COLUMN blocked_reason TEXT",
                    ),
                    (
                        "blocked_since",
                        "ALTER TABLE jobs ADD COLUMN blocked_since INTEGER",
                    ),
                    (
                        "eligible_since",
                        "ALTER TABLE jobs ADD COLUMN eligible_since INTEGER NOT NULL DEFAULT 0",
                    ),
                    (
                        "failure_reason",
                        "ALTER TABLE jobs ADD COLUMN failure_reason TEXT",
                    ),
                    (
                        "excluded_nodes_json",
                        "ALTER TABLE jobs ADD COLUMN excluded_nodes_json TEXT NOT NULL DEFAULT '[]'",
                    ),
                ] {
                    if !job_column_exists(&tx, name)? {
                        tx.execute(ddl, []).map_err(internal)?;
                    }
                }
                tx.execute(
                    "UPDATE jobs SET eligible_since=created_at WHERE eligible_since=0",
                    [],
                )
                .map_err(internal)?;
                // Additive table for HTTP request keys. Existing rows are untouched.
                ensure_request_keys(&tx)?;
                tx.execute(
                    "UPDATE schema_version SET version = ?1",
                    params![CURRENT_SCHEMA_VERSION],
                )
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

/// Creates the HTTP request-key table for databases written before schema 7. No existing row is
/// changed, so this is additive and safe to re-run. Kept for every older schema migration.
fn ensure_request_keys(tx: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS request_keys (
            operator TEXT NOT NULL,
            key TEXT NOT NULL,
            operation TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            status INTEGER NOT NULL,
            response TEXT,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            PRIMARY KEY (operator, key)
        );",
    )
    .map_err(internal)
}

/// True when `jobs` already has `name`. Used by the v1-v3 migrations so a partially upgraded
/// database does not fail with `duplicate column name`.
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
            patch_secret_hits_json TEXT NOT NULL DEFAULT '{\"count\":0,\"rules\":[]}',
            parent_job_id TEXT,
            root_job_id TEXT,
            depth INTEGER NOT NULL DEFAULT 0,
            publish_json TEXT,
            staged_commit TEXT,
            blocked_reason TEXT,
            blocked_since INTEGER,
            eligible_since INTEGER NOT NULL DEFAULT 0,
            failure_reason TEXT,
            excluded_nodes_json TEXT NOT NULL DEFAULT '[]'
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
        -- HTTP request keys, distinct from job-spec `idempotency`. One row per
        -- (operator, key) with a bound operation/body fingerprint and expiring stored response.
        CREATE TABLE request_keys (
            operator TEXT NOT NULL,
            key TEXT NOT NULL,
            operation TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            status INTEGER NOT NULL,
            response TEXT,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            PRIMARY KEY (operator, key)
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
        CordonSource::Health => {
            let cooldown = completion
                .retry_after_secs
                .map(|s| format!(" (retry after {s}s)"))
                .unwrap_or_default();
            format!("Provider gateway cooldown for {provider}{cooldown}: {detail}")
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
    let spec: JobSpec = decode_spec(&spec_json)?;
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

/// Blocked jobs that depend on `dep`. `only_failed_reason` narrows this to the jobs waiting on a
/// dead predecessor, which are the ones a retry, TTL or cancel cascade may touch.
fn blocked_dependents_tx(
    tx: &rusqlite::Transaction<'_>,
    dep: &str,
    only_failed_reason: bool,
) -> Result<Vec<(String, JobSpec)>, StoreError> {
    let sql = if only_failed_reason {
        "SELECT j.id,j.spec_json FROM jobs j JOIN job_deps d ON d.job=j.id
         WHERE d.dep=?1 AND j.state='blocked' AND j.blocked_reason='predecessor failed'
         ORDER BY j.id"
    } else {
        "SELECT j.id,j.spec_json FROM jobs j JOIN job_deps d ON d.job=j.id
         WHERE d.dep=?1 AND j.state='blocked' ORDER BY j.id"
    };
    let mut stmt = tx.prepare(sql).map_err(internal)?;
    let rows = stmt
        .query_map(params![dep], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(internal)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, json) = row.map_err(internal)?;
        let spec: JobSpec = decode_spec(&json)?;
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
             summary=NULL,finished_at=NULL,blocked_reason=NULL,blocked_since=NULL,
             eligible_since=?3
             WHERE id=?1 AND state='blocked'",
            params![id, now.wall_unix, now.wall_unix],
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
    for (id, spec) in blocked_dependents_tx(tx, dep, false)? {
        release_blocked_job(tx, &id, spec.include_failed, now)?;
    }
    Ok(())
}

/// A dead predecessor does not fail its dependents outright: they stay `blocked` with a reason and
/// a wall-clock stamp so a retry can reattach them, or the TTL maintenance pass can fail them.
/// The block propagates transitively along the dependency graph; jobs that opted into failed inputs
/// are still released as before.
fn fail_dependents(
    tx: &rusqlite::Transaction<'_>,
    dep: &str,
    dep_state: &str,
    now: Now,
) -> Result<(), StoreError> {
    let mut queue = vec![(dep.to_owned(), dep_state.to_owned())];
    while let Some((dep_id, dep_state)) = queue.pop() {
        for (id, spec) in blocked_dependents_tx(tx, &dep_id, false)? {
            // Opting into failed inputs means the dependency is data, not a reason to block.
            if spec.include_failed {
                release_blocked_job(tx, &id, spec.include_failed, now)?;
                continue;
            }
            let summary = format!("dependency {dep_id} {dep_state}; waiting for retry or TTL");
            tx.execute(
                "UPDATE jobs SET state='blocked',blocked_reason='predecessor failed',
                 blocked_since=?2,updated_at=?2,summary=?3,failure_class=NULL,
                 finished_at=NULL WHERE id=?1 AND state='blocked'",
                params![id, now.wall_unix, summary],
            )
            .map_err(internal)?;
            // The dependent itself is now a blocked predecessor for the next level.
            queue.push((id, JobState::Blocked.as_str().to_owned()));
        }
    }
    Ok(())
}

/// The predecessor named in a blocked job's summary, falling back to its first unfinished dep.
fn blocked_predecessor(rec: &JobRecord) -> Option<&str> {
    rec.summary
        .as_deref()
        .and_then(|summary| summary.strip_prefix("dependency "))
        .and_then(|rest| rest.split_whitespace().next())
        .or_else(|| rec.waiting_on.first().map(|id| id.as_str()))
}

/// Git object names are exactly 40 lowercase hex digits; nothing else is accepted.
fn valid_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Shared cancel write. The bool says whether the job actually became `cancelled` (rather than
/// merely getting a cancel request), which is when a dependent cascade is meaningful.
fn cancel_job_tx(
    tx: &rusqlite::Transaction<'_>,
    id: &JobId,
    now: Now,
) -> Result<(JobRecord, bool), StoreError> {
    let (mut rec, lease, recov) = get_job_tx(tx, id.as_str())?;
    let cancelled_now = match rec.state.cancel_effect() {
        CancelEffect::Now => {
            check_transition(rec.state, JobState::Cancelled)?;
            rec.state = JobState::Cancelled;
            rec.updated_at = now.wall_unix;
            rec.finished_at = Some(now.wall_unix);
            update_job_tx(tx, &rec, None, None)?;
            fail_dependents(tx, rec.id.as_str(), JobState::Cancelled.as_str(), now)?;
            true
        }
        CancelEffect::Request => {
            rec.cancel_requested = true;
            rec.updated_at = now.wall_unix;
            update_job_tx(tx, &rec, lease, recov)?;
            false
        }
        CancelEffect::Refuse => return Err(StoreError::Conflict { length: None }),
    };
    rec.waiting_on = load_waiting(tx, rec.id.as_str())?;
    Ok((rec, cancelled_now))
}

/// Cancels the blocked dependents that a cancelled predecessor left waiting for a retry.
fn cancel_blocked_dependents_tx(
    tx: &rusqlite::Transaction<'_>,
    root: &JobId,
    now: Now,
) -> Result<(), StoreError> {
    let mut queue = vec![root.clone()];
    let mut seen = BTreeSet::new();
    while let Some(parent) = queue.pop() {
        for (id, _) in blocked_dependents_tx(tx, parent.as_str(), true)? {
            let id = JobId::parse(&id).map_err(internal)?;
            if !seen.insert(id.clone()) {
                continue;
            }
            let (mut rec, _, _) = get_job_tx(tx, id.as_str())?;
            if rec.state != JobState::Blocked {
                continue;
            }
            check_transition(rec.state, JobState::Cancelled)?;
            rec.state = JobState::Cancelled;
            rec.finished_at = Some(now.wall_unix);
            rec.updated_at = now.wall_unix;
            rec.blocked_reason = None;
            rec.blocked_since = None;
            update_job_tx(tx, &rec, None, None)?;
            queue.push(id);
        }
    }
    Ok(())
}

/// Points an idempotency key at the newest job for it. A retry deliberately overwrites the old
/// mapping so submitting the same key again does not resurrect the finished job.
fn put_idempotency(
    tx: &rusqlite::Transaction<'_>,
    spec: &JobSpec,
    job_id: &JobId,
) -> Result<(), StoreError> {
    if let Some(key) = &spec.idempotency_key {
        tx.execute(
            "INSERT INTO idempotency(key,spec_hash,job_id) VALUES (?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET spec_hash=excluded.spec_hash, job_id=excluded.job_id",
            params![key, spec.canonical_json(), job_id.as_str()],
        )
        .map_err(internal)?;
    }
    Ok(())
}

/// Blocked dependents to restore for a chained retry, in dependency order: a job is only returned
/// after every predecessor in the captured set has been returned.
fn chain_dependents_tx(
    tx: &rusqlite::Transaction<'_>,
    root: &JobId,
) -> Result<Vec<JobId>, StoreError> {
    let mut captured: BTreeSet<JobId> = BTreeSet::new();
    let mut queue = vec![root.clone()];
    while let Some(parent) = queue.pop() {
        for (id, _) in blocked_dependents_tx(tx, parent.as_str(), true)? {
            let id = JobId::parse(&id).map_err(internal)?;
            if captured.insert(id.clone()) {
                queue.push(id);
            }
        }
    }
    let mut ordered = Vec::new();
    while !captured.is_empty() {
        let mut progressed = false;
        let ids: Vec<JobId> = captured.iter().cloned().collect();
        for id in ids {
            let parents: Vec<JobId> = {
                let mut stmt = tx
                    .prepare("SELECT dep FROM job_deps WHERE job=?1")
                    .map_err(internal)?;
                let rows = stmt
                    .query_map(params![id.as_str()], |r| r.get::<_, String>(0))
                    .map_err(internal)?;
                let mut v = Vec::new();
                for row in rows {
                    v.push(JobId::parse(&row.map_err(internal)?).map_err(internal)?);
                }
                v
            };
            if parents.iter().all(|dep| !captured.contains(dep)) {
                ordered.push(id.clone());
                captured.remove(&id);
                progressed = true;
            }
        }
        if !progressed {
            return Err(internal("dependency cycle in blocked dependents"));
        }
    }
    Ok(ordered)
}

/// Requeues a terminal job in place for one more attempt. Operator retry is an explicit escape
/// from the state table: terminal jobs have no normal outgoing edges.
fn requeue_retry_root_tx(
    tx: &rusqlite::Transaction<'_>,
    id: &JobId,
    spec: &JobSpec,
    now: Now,
) -> Result<JobRecord, StoreError> {
    let (mut rec, _, _) = get_job_tx(tx, id.as_str())?;
    if !rec.state.is_retryable() {
        return Err(StoreError::NotRetryable(rec.state));
    }

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
            Some("failed" | "cancelled") => return Err(StoreError::DeadDependency(dep.clone())),
            Some(_) => {}
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
        if depends_on(tx, dep.as_str(), id.as_str())? {
            return Err(StoreError::DependencyCycle);
        }
    }
    tx.execute("DELETE FROM job_deps WHERE job=?1", params![id.as_str()])
        .map_err(internal)?;
    for (ord, dep) in spec.after.iter().enumerate() {
        tx.execute(
            "INSERT INTO job_deps(job,dep,ord) VALUES (?1,?2,?3)",
            params![id.as_str(), dep.as_str(), ord as i64],
        )
        .map_err(internal)?;
    }
    let blocked = !deps_satisfied(tx, id.as_str(), spec.include_failed)?;

    rec.spec = spec.clone();
    rec.state = if blocked {
        JobState::Blocked
    } else {
        JobState::Queued
    };
    rec.attempt = rec.attempt.saturating_add(1);
    rec.node = None;
    rec.assigned_provider = None;
    rec.assigned_model = None;
    rec.updated_at = now.wall_unix;
    rec.eligible_since = now.wall_unix;
    rec.started_at = None;
    rec.finished_at = None;
    rec.failure_class = None;
    rec.failure_reason = None;
    rec.excluded_nodes.clear();
    rec.summary = None;
    rec.exit_code = None;
    rec.artifacts = Vec::new();
    rec.cancel_requested = false;
    rec.log_len = 0;
    rec.diff_bytes = None;
    rec.expect_failed = false;
    rec.scrubbed_hits = 0;
    rec.patch_scrubbed = false;
    rec.patch_secret_hits = lu_proto::view::PatchSecretHitsView::default();
    rec.staged_commit = None;
    rec.blocked_reason = None;
    rec.blocked_since = None;
    rec.publish = None;
    rec.waiting_on = load_waiting(tx, id.as_str())?;
    update_job_tx(tx, &rec, None, None)?;
    put_idempotency(tx, spec, id)?;
    Ok(rec)
}

/// Restores blocked dependents after the root has been requeued. They keep their ids and edges;
/// a dependent with all dependencies satisfied leaves `blocked`, while the rest stay blocked until
/// the retried predecessor finishes. Cancelled or failed dependents are terminal and are untouched.
fn restore_retry_dependents_tx(
    tx: &rusqlite::Transaction<'_>,
    root: &JobId,
    chain: bool,
    now: Now,
) -> Result<Vec<JobId>, StoreError> {
    let ids: Vec<JobId> = if chain {
        chain_dependents_tx(tx, root)?
    } else {
        blocked_dependents_tx(tx, root.as_str(), true)?
            .into_iter()
            .map(|(id, _)| JobId::parse(&id).map_err(internal))
            .collect::<Result<_, _>>()?
    };
    let mut requeued = Vec::new();
    for id in ids {
        let (mut rec, lease, recov) = get_job_tx(tx, id.as_str())?;
        if rec.state != JobState::Blocked
            || rec.blocked_reason.as_deref() != Some("predecessor failed")
        {
            continue;
        }
        rec.blocked_reason = None;
        rec.blocked_since = None;
        rec.summary = None;
        rec.failure_class = None;
        rec.failure_reason = None;
        rec.finished_at = None;
        rec.updated_at = now.wall_unix;
        if deps_satisfied(tx, id.as_str(), rec.spec.include_failed)? {
            release_blocked_job(tx, id.as_str(), rec.spec.include_failed, now)?;
        } else {
            rec.waiting_on = load_waiting(tx, id.as_str())?;
            update_job_tx(tx, &rec, lease, recov)?;
        }
        requeued.push(id);
    }
    Ok(requeued)
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

/// Checks an `If-Match` observation inside the caller's write transaction. The controller derives
/// the expected value from the same body hash a GET returns; `updated_at` is the row version
/// observed with that body. The check and the write share one transaction here.
fn check_expected_tx(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    expected: Option<i64>,
) -> Result<(), StoreError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let current: Option<i64> = tx
        .query_row(
            "SELECT updated_at FROM jobs WHERE id=?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?;
    if current != Some(expected) {
        Err(StoreError::Precondition)
    } else {
        Ok(())
    }
}

fn get_job_tx(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
) -> Result<(JobRecord, Option<u64>, Option<u64>), StoreError> {
    let mut stmt = tx
        .prepare("SELECT id,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,diff_bytes,lease_until_mono_ms,recovering_since_mono_ms,assigned_provider,assigned_model,expect_failed,scrubbed_hits,patch_scrubbed,patch_secret_hits_json,parent_job_id,root_job_id,depth,publish_json,staged_commit,blocked_reason,blocked_since,eligible_since,failure_reason,excluded_nodes_json FROM jobs WHERE id=?1")
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
                r.get::<_, String>("patch_secret_hits_json")?,
                r.get::<_, Option<String>>("parent_job_id")?,
                r.get::<_, Option<String>>("root_job_id")?,
                r.get::<_, i64>("depth")?,
                r.get::<_, Option<String>>("publish_json")?,
                r.get::<_, Option<String>>("staged_commit")?,
                r.get::<_, Option<String>>("blocked_reason")?,
                r.get::<_, Option<i64>>("blocked_since")?,
                r.get::<_, i64>("eligible_since")?,
                r.get::<_, Option<String>>("failure_reason")?,
                r.get::<_, String>("excluded_nodes_json")?,
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
        patch_secret_hits_json,
        parent_job_id,
        root_job_id,
        depth,
        publish_json,
        staged_commit,
        blocked_reason,
        blocked_since,
        eligible_since,
        failure_reason_s,
        excluded_nodes_json,
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
        eligible_since,
        started_at,
        finished_at,
        failure_class,
        failure_reason: parse_failure_reason(failure_reason_s)?,
        excluded_nodes: decode_excluded_nodes(&excluded_nodes_json)?,
        summary,
        exit_code,
        artifacts,
        cancel_requested: cancel_i != 0,
        log_len: log_len_i as u64,
        diff_bytes: diff_bytes.map(|v| v as u64),
        expect_failed: expect_failed != 0,
        scrubbed_hits: scrubbed_hits as u64,
        patch_scrubbed: patch_scrubbed != 0,
        patch_secret_hits: serde_json::from_str(&patch_secret_hits_json).unwrap_or_default(),
        waiting_on: load_waiting(tx, &id_s)?,
        staged_commit,
        blocked_reason,
        blocked_since,
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
    let patch_secret_hits_json = serde_json::to_string(&rec.patch_secret_hits).map_err(internal)?;
    let excluded_nodes_json = encode_excluded_nodes(&rec.excluded_nodes)?;
    tx.execute(
        "UPDATE jobs SET state=?2,spec_json=?3,attempt=?4,node=?5,epoch=?6,created_at=?7,updated_at=?8,started_at=?9,finished_at=?10,failure_class=?11,summary=?12,exit_code=?13,artifacts_json=?14,cancel_requested=?15,log_len=?16,lease_until_mono_ms=?17,recovering_since_mono_ms=?18,assigned_provider=?19,assigned_model=?20,diff_bytes=?21,expect_failed=?22,scrubbed_hits=?23,patch_scrubbed=?24,parent_job_id=?25,root_job_id=?26,depth=?27,publish_json=?28,staged_commit=?29,blocked_reason=?30,blocked_since=?31,patch_secret_hits_json=?32,eligible_since=?33,failure_reason=?34,excluded_nodes_json=?35 WHERE id=?1",
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
            rec.staged_commit,
            rec.blocked_reason,
            rec.blocked_since,
            patch_secret_hits_json,
            rec.eligible_since,
            rec.failure_reason.map(NodeFailureReason::as_str),
            excluded_nodes_json,
        ],
    ).map_err(internal)?;
    Ok(())
}

fn node_offers_provider(hb: &Heartbeat, provider: &str) -> bool {
    hb.providers
        .iter()
        .any(|p| p.name == provider && matches!(p.state, lu_proto::ProviderState::Ok))
}

fn refresh_health_cordons(
    tx: &rusqlite::Transaction<'_>,
    hb: &Heartbeat,
) -> Result<(), StoreError> {
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

/// Inserts a fresh job and its dependency edges. Callers own the idempotency write so a retry can
/// point an existing key at the new row instead of resurrecting the old one.
fn expire_drafts_in_tx(
    tx: &rusqlite::Transaction<'_>,
    count: u64,
    now: Now,
    origin: Option<&str>,
) -> Result<u64, StoreError> {
    if count == 0 {
        return Ok(0);
    }
    let ids: Vec<String> = {
        let limit = count.min(i64::MAX as u64) as i64;
        if let Some(origin) = origin {
            let mut stmt = tx
                .prepare(
                    "SELECT id FROM jobs WHERE state='draft' AND COALESCE(json_extract(spec_json, '$.origin'), '') = ?2 ORDER BY created_at, rowid LIMIT ?1",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![limit, origin], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(internal)?
        } else {
            let mut stmt = tx
                .prepare(
                    "SELECT id FROM jobs WHERE state='draft' ORDER BY created_at, rowid LIMIT ?1",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![limit], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(internal)?
        }
    };
    for id in &ids {
        let (mut rec, lease, recov) = get_job_tx(tx, id)?;
        check_transition(rec.state, JobState::Expired)?;
        rec.state = JobState::Expired;
        rec.updated_at = now.wall_unix;
        rec.finished_at = Some(now.wall_unix);
        update_job_tx(tx, &rec, lease, recov)?;
    }
    Ok(ids.len() as u64)
}

fn insert_job_tx(
    tx: &rusqlite::Transaction<'_>,
    spec: &JobSpec,
    now: Now,
    lineage: Option<(&JobId, &JobId, u32)>,
) -> Result<JobRecord, StoreError> {
    let id = JobId::new();
    // Every job is the root of its own spawn tree until a controller explicitly parents it.
    // Keeping the root id on top-level jobs makes the lineage self-describing and lets the
    // descendant count subtract exactly one row.
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
            Some("failed" | "cancelled") => return Err(StoreError::DeadDependency(dep.clone())),
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
    let canonical = spec.canonical_json();
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
        eligible_since: now.wall_unix,
        started_at: None,
        finished_at: None,
        failure_class: None,
        failure_reason: None,
        excluded_nodes: Vec::new(),
        summary: None,
        exit_code: None,
        artifacts: Vec::new(),
        cancel_requested: false,
        log_len: 0,
        diff_bytes: None,
        expect_failed: false,
        scrubbed_hits: 0,
        patch_scrubbed: false,
        patch_secret_hits: lu_proto::view::PatchSecretHitsView::default(),
        waiting_on: Vec::new(),
        staged_commit: None,
        blocked_reason: None,
        blocked_since: None,
        parent_job_id: lineage.map(|(parent, _, _)| parent.clone()),
        root_job_id: Some(root_job_id.clone()),
        depth: lineage.map(|(_, _, depth)| depth).unwrap_or(0),
        publish: None,
    };
    tx.execute(
        "INSERT INTO jobs(id,dispatch_seq,state,spec_json,attempt,node,epoch,created_at,updated_at,started_at,finished_at,failure_class,summary,exit_code,artifacts_json,cancel_requested,log_len,lease_until_mono_ms,recovering_since_mono_ms,parent_job_id,root_job_id,depth,eligible_since)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
        params![
            rec.id.as_str(), dispatch_seq, state_str(rec.state), canonical, 1i64, Option::<String>::None, 0i64,
            rec.created_at, rec.updated_at, Option::<i64>::None, Option::<i64>::None,
            Option::<String>::None, Option::<String>::None, Option::<i32>::None,
            encode_artifacts(&[]), 0i64, 0i64, Option::<i64>::None, Option::<i64>::None,
            rec.parent_job_id.as_ref().map(|id| id.as_str()),
            rec.root_job_id.as_ref().map(|id| id.as_str()),
            rec.depth as i64,
            rec.eligible_since,
        ],
    ).map_err(internal)?;
    for (ord, dep) in spec.after.iter().enumerate() {
        tx.execute(
            "INSERT INTO job_deps(job,dep,ord) VALUES (?1,?2,?3)",
            params![rec.id.as_str(), dep.as_str(), ord as i64],
        )
        .map_err(internal)?;
    }
    rec.waiting_on = load_waiting(tx, rec.id.as_str())?;
    Ok(rec)
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
        require_absent: bool,
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
                if require_absent {
                    return Err(StoreError::Precondition);
                }
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
        let rec = insert_job_tx(&tx, spec, now, lineage)?;
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

/// In-memory builder for one [`StatsAggregate`]. Durations are kept as samples until the report is
/// finalised so medians, p90s and cumulative buckets all come from one pass over the same rows.
struct StatsAccumulator {
    aggregate: StatsAggregate,
    runtime_samples: Vec<u64>,
    queue_samples: Vec<u64>,
    repositories: BTreeMap<String, u64>,
}

#[derive(Clone, Copy)]
struct StatsJob<'a> {
    state: JobState,
    failure: Option<FailureClass>,
    attempt: u32,
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    repository: Option<&'a str>,
}

impl StatsAccumulator {
    fn new() -> Self {
        Self {
            aggregate: StatsAggregate::empty(),
            runtime_samples: Vec::new(),
            queue_samples: Vec::new(),
            repositories: BTreeMap::new(),
        }
    }

    fn add_job(&mut self, job: StatsJob<'_>) {
        let StatsJob {
            state,
            failure,
            attempt,
            created_at,
            started_at,
            finished_at,
            repository,
        } = job;
        *self
            .aggregate
            .states
            .entry(state.as_str().to_owned())
            .or_insert(0) += 1;
        self.aggregate.retries = self
            .aggregate
            .retries
            .saturating_add(attempt.saturating_sub(1) as u64);
        if state == JobState::Failed {
            // Failed jobs written before failure classes existed still need a fixed label.
            let class = failure.unwrap_or(FailureClass::Node);
            *self
                .aggregate
                .failures
                .entry(class.as_str().to_owned())
                .or_insert(0) += 1;
        }
        if let Some(finished) = finished_at {
            self.aggregate.last_finished_at = Some(
                self.aggregate
                    .last_finished_at
                    .map_or(finished, |latest| latest.max(finished)),
            );
            // Busy time is wall-clock job duration, not CPU time. `started_at` is the best basis;
            // older rows without it fall back to creation time. Missing `finished_at` means no
            // duration sample, but the job still contributes to counts, retries and repositories.
            let began = started_at.unwrap_or(created_at);
            self.runtime_samples
                .push(finished.saturating_sub(began).max(0) as u64);
        }
        if let Some(started) = started_at {
            self.queue_samples
                .push(started.saturating_sub(created_at).max(0) as u64);
        }
        if let Some(repository) = repository {
            *self.repositories.entry(repository.to_owned()).or_insert(0) += 1;
        }
    }

    fn finish(mut self) -> StatsAggregate {
        let runtime = summarize(std::mem::take(&mut self.runtime_samples));
        let queue_wait = summarize(std::mem::take(&mut self.queue_samples));
        self.aggregate.busy_seconds = runtime.sum_seconds;
        self.aggregate.runtime = runtime;
        self.aggregate.queue_wait = queue_wait;
        let mut repos: Vec<RepoContribution> = self
            .repositories
            .into_iter()
            .map(|(repository, jobs)| RepoContribution { repository, jobs })
            .collect();
        repos.sort_by(|a, b| {
            b.jobs
                .cmp(&a.jobs)
                .then_with(|| a.repository.cmp(&b.repository))
        });
        repos.truncate(TOP_REPOSITORIES);
        self.aggregate.top_repositories = repos;
        self.aggregate
    }
}

fn summarize(samples: Vec<u64>) -> DurationSummary {
    let mut samples = samples;
    samples.sort_unstable();
    let count = samples.len() as u64;
    let sum_seconds = samples
        .iter()
        .fold(0u64, |total, value| total.saturating_add(*value));
    let mut buckets: BTreeMap<String, u64> = STATS_BUCKET_LABELS
        .iter()
        .map(|label| ((*label).to_owned(), 0))
        .collect();
    for sample in &samples {
        for (label, bound) in STATS_BUCKET_LABELS.iter().zip(STATS_BUCKETS_SECS.iter()) {
            if *sample <= *bound {
                *buckets.entry((*label).to_owned()).or_insert(0) += 1;
            }
        }
        *buckets.entry("+Inf".to_owned()).or_insert(0) += 1;
    }
    DurationSummary {
        count,
        sum_seconds,
        median_seconds: nearest_rank(&samples, 50),
        p90_seconds: nearest_rank(&samples, 90),
        buckets,
    }
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = percentile.saturating_mul(sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// A queued job as seen by one dispatch poll. Provider choices are computed with the heartbeat and
/// cordons from the same short transaction, so the expensive calendar work has all its inputs
/// without holding the write lock.
#[derive(Debug)]
struct DispatchCandidate {
    id: String,
    dispatch_seq: i64,
    spec: JobSpec,
    /// The parent's group for a scoped child, whose own `spec.group` is reset to `None`.
    group: Option<String>,
    eligible_since: i64,
    choices: Vec<ProviderChoice>,
}

#[derive(Debug)]
struct DispatchSnapshot {
    limits: HashMap<String, usize>,
    candidates: Vec<DispatchCandidate>,
}

/// Node checks that must be atomic with the lease. Returns `None` when this node cannot take work.
fn dispatch_node(
    tx: &rusqlite::Transaction<'_>,
    node: &NodeName,
    limits: &crate::DispatchLimits,
) -> Result<Option<Heartbeat>, StoreError> {
    let row: Option<(String, i64)> = tx
        .query_row(
            "SELECT heartbeat_json, draining FROM nodes WHERE name=?1",
            params![node.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((json, draining)) = row else {
        return Ok(None);
    };
    if draining != 0 {
        return Ok(None);
    }
    let hb: Heartbeat = serde_json::from_str(&json).map_err(internal)?;
    if !matches!(hb.harness, lu_proto::HarnessState::Idle) {
        return Ok(None);
    }
    if crate::heartbeat_health_reason(node, &hb, limits).is_some() {
        return Ok(None);
    }
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
    Ok(Some(hb))
}

fn active_group_counts(
    tx: &rusqlite::Transaction<'_>,
) -> Result<HashMap<String, usize>, StoreError> {
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
    Ok(counts)
}

/// One provider/model pair a job may run on; `(None, None)` means run without an assignment.
type ProviderChoice = (Option<String>, Option<String>);

/// Provider choices in preference order. An `any` job uses the healthy pairs the heartbeat
/// published; a named provider uses its primary then its fallbacks. A job with no provider at all
/// returns one empty choice, which means "run without an assignment".
fn provider_choices(
    tx: &rusqlite::Transaction<'_>,
    node: &NodeName,
    hb: &Heartbeat,
    spec: &JobSpec,
    now: Now,
) -> Result<Vec<ProviderChoice>, StoreError> {
    let choices: Vec<(String, Option<String>)> = if spec.is_any_provider() {
        hb.provider_models
            .iter()
            .filter(|pair| node_offers_provider(hb, &pair.provider))
            .map(|pair| (pair.provider.clone(), Some(pair.model.clone())))
            .collect()
    } else {
        match &spec.provider {
            Some(primary) => std::iter::once((primary.clone(), spec.model.clone()))
                .chain(spec.fallbacks.iter().map(|choice| {
                    (
                        choice.provider.clone(),
                        choice.model.clone().or_else(|| spec.model.clone()),
                    )
                }))
                .collect(),
            None => Vec::new(),
        }
    };
    if choices.is_empty() {
        return Ok(if spec.provider.is_none() {
            vec![(None, None)]
        } else {
            Vec::new()
        });
    }
    let mut usable = Vec::new();
    for (provider, model) in choices {
        let cordoned: Option<i64> = tx
            .query_row(
                "SELECT until_mono_ms FROM cordons WHERE node=?1 AND provider=?2",
                params![node.as_str(), provider],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        if cordoned.is_some_and(|until| (until as u64) > now.mono_ms) {
            continue;
        }
        if node_offers_provider(hb, &provider) {
            usable.push((Some(provider), model));
        }
    }
    Ok(usable)
}

/// Calendar gates use the parent's group even though `submit_child` resets the child's own group.
/// The root's group is the fallback for grandchildren, whose immediate parent is also ungrouped.
fn inherited_group(
    tx: &rusqlite::Transaction<'_>,
    parent: Option<String>,
    root: Option<String>,
) -> Result<Option<String>, StoreError> {
    let mut stmt = tx
        .prepare("SELECT spec_json FROM jobs WHERE id=?1")
        .map_err(internal)?;
    for id in [parent, root].into_iter().flatten() {
        let spec: Option<String> = stmt
            .query_row(params![id], |row| row.get(0))
            .optional()
            .map_err(internal)?;
        if let Some(spec) = spec {
            let spec = decode_spec(&spec)?;
            if spec.group.is_some() {
                return Ok(spec.group);
            }
        }
    }
    Ok(None)
}

fn dispatch_snapshot(
    store: &SqliteStore,
    node: &NodeName,
    now: Now,
    schedule: &lu_schedule::Schedule,
    limits: &crate::DispatchLimits,
) -> Result<Option<DispatchSnapshot>, StoreError> {
    let mut conn = store.conn.lock().map_err(|_| internal("mutex poisoned"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    let hb = match dispatch_node(&tx, node, limits)? {
        Some(hb) => hb,
        None => {
            tx.commit().map_err(internal)?;
            return Ok(None);
        }
    };
    let limits = store
        .group_limits
        .lock()
        .map_err(|_| internal("mutex poisoned"))?
        .clone();
    let active = active_group_counts(&tx)?;
    let mut candidates = Vec::new();
    {
        let mut stmt = tx
            .prepare(
                "SELECT id,dispatch_seq,spec_json,eligible_since,parent_job_id,root_job_id,excluded_nodes_json \
                 FROM jobs WHERE state='queued'",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(internal)?;
        let mut raw = Vec::new();
        for row in rows {
            raw.push(row.map_err(internal)?);
        }
        for (id, dispatch_seq, spec_json, eligible_since, parent, root, excluded_json) in raw {
            if decode_excluded_nodes(&excluded_json)?.contains(node) {
                continue;
            }
            let spec = decode_spec(&spec_json)?;
            let group = match &spec.group {
                Some(group) => Some(group.clone()),
                None => inherited_group(&tx, parent.clone(), root.clone())?,
            };
            if let Some(group) = &spec.group {
                if let Some(&limit) = limits.get(group) {
                    if active.get(group).copied().unwrap_or(0) >= limit {
                        continue;
                    }
                }
            }
            if !crate::labels_satisfied(&spec.requires, &hb.labels)
                || !crate::taints_tolerated(&hb.taints, &spec.tolerates)
            {
                continue;
            }
            let choices = provider_choices(&tx, node, &hb, &spec, now)?;
            if choices.is_empty() {
                continue;
            }
            candidates.push(DispatchCandidate {
                id,
                dispatch_seq,
                spec,
                group,
                eligible_since,
                choices,
            });
        }
    }
    tx.commit().map_err(internal)?;
    candidates.sort_by(|a, b| {
        let ka = lu_schedule::queue_key(
            now.wall_unix,
            a.eligible_since,
            a.spec.priority,
            a.dispatch_seq.max(0) as u64,
            schedule.defaults.starvation_secs,
        );
        let kb = lu_schedule::queue_key(
            now.wall_unix,
            b.eligible_since,
            b.spec.priority,
            b.dispatch_seq.max(0) as u64,
            schedule.defaults.starvation_secs,
        );
        // Starvation class and effective priority sort high to low; ties keep the oldest
        // eligibility and submission order so the queue stays total and restart-stable.
        kb.0.cmp(&ka.0)
            .then_with(|| kb.1.cmp(&ka.1))
            .then_with(|| ka.3.cmp(&kb.3))
            .then_with(|| ka.2.cmp(&kb.2))
    });
    // A per-poll budget keeps an enormous queue bounded even when all its jobs fail the same
    // calendar gate. The default is deliberately far above normal queue sizes.
    candidates.truncate(schedule.defaults.step_budget.min(candidates.len()));
    Ok(Some(DispatchSnapshot { limits, candidates }))
}

/// Inputs one dispatch poll shares between evaluation and the final short transaction.
#[derive(Clone, Copy)]
struct DispatchPoll<'a> {
    node: &'a NodeName,
    now: Now,
    lease_ms: u64,
    schedule: &'a lu_schedule::Schedule,
    limits: &'a crate::DispatchLimits,
    snapshot: &'a DispatchSnapshot,
}

/// Final short transaction: re-check everything that can race with the evaluation, then lease.
fn commit_dispatch(
    store: &SqliteStore,
    poll: DispatchPoll<'_>,
    candidate: &DispatchCandidate,
    choice: &ProviderChoice,
) -> Result<Option<Assignment>, StoreError> {
    let DispatchPoll {
        node,
        now,
        lease_ms,
        schedule,
        limits,
        snapshot,
    } = poll;
    let (provider, model) = choice;
    let mut conn = store.conn.lock().map_err(|_| internal("mutex poisoned"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    if dispatch_node(&tx, node, limits)?.is_none() {
        tx.commit().map_err(internal)?;
        return Ok(None);
    }
    let (mut rec, _, _) = match get_job_tx(&tx, &candidate.id) {
        Ok(value) => value,
        Err(StoreError::NotFound) => {
            tx.commit().map_err(internal)?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if rec.state != JobState::Queued || rec.excluded_nodes.contains(node) {
        tx.commit().map_err(internal)?;
        return Ok(None);
    }
    if let Some(group) = &rec.spec.group {
        if let Some(&limit) = snapshot.limits.get(group) {
            let active = active_group_counts(&tx)?;
            if active.get(group).copied().unwrap_or(0) >= limit {
                tx.commit().map_err(internal)?;
                return Ok(None);
            }
        }
    }
    // Provider cordons are wall-clock independent; a heartbeat between the two transactions
    // may have cordoned the chosen pair, so check it once more.
    if let Some(provider) = provider {
        let cordoned: Option<i64> = tx
            .query_row(
                "SELECT until_mono_ms FROM cordons WHERE node=?1 AND provider=?2",
                params![node.as_str(), provider],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        if cordoned.is_some_and(|until| (until as u64) > now.mono_ms) {
            tx.commit().map_err(internal)?;
            return Ok(None);
        }
    }
    // The snapshot was evaluated without the write lock. Re-check the same admission decision
    // here so a boundary crossed while other polls held the writer cannot lease a gated job.
    let job_schedule = rec
        .spec
        .schedule
        .clone()
        .unwrap_or_else(lu_schedule::JobSchedule::default);
    let request = lu_schedule::AdmissionRequest {
        now: now.wall_unix,
        eligible_since: rec.eligible_since,
        timeout_secs: rec.spec.timeout_secs as i64,
        job: job_schedule,
        node: Some(node.as_str().to_owned()),
        group: candidate.group.clone(),
        provider: provider.clone(),
        model: model.clone(),
    };
    match schedule.admit(&request) {
        Ok(lu_schedule::Admission::Admitted(_)) => {}
        Ok(lu_schedule::Admission::Blocked(_)) | Err(_) => {
            tx.commit().map_err(internal)?;
            return Ok(None);
        }
    }
    check_transition(rec.state, JobState::Leased)?;
    rec.state = JobState::Leased;
    rec.node = Some(node.clone());
    rec.assigned_provider = provider.clone();
    rec.assigned_model = model.clone();
    rec.epoch = rec.epoch.saturating_add(1);
    rec.updated_at = now.wall_unix;
    rec.failure_class = None;
    rec.failure_reason = None;
    rec.summary = None;
    rec.exit_code = None;
    rec.finished_at = None;
    update_job_tx(&tx, &rec, Some(now.mono_ms.saturating_add(lease_ms)), None)?;
    tx.commit().map_err(internal)?;
    Ok(Some(Assignment {
        job_id: rec.id,
        epoch: rec.epoch,
        provider: rec.assigned_provider.clone(),
        model: rec.assigned_model.clone(),
        spec: rec.spec,
    }))
}

fn wall_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

impl Store for SqliteStore {
    fn reserve_request_key(
        &self,
        operator: &str,
        key: &str,
        operation: &str,
        fingerprint: &str,
        ttl_secs: u64,
    ) -> Result<RequestKeyReservation, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let now = wall_secs();
        // An expired key frees the slot; a completed key with the same fingerprint replays.
        tx.execute(
            "DELETE FROM request_keys WHERE operator=?1 AND key=?2 AND expires_at<=?3",
            params![operator, key, now],
        )
        .map_err(internal)?;
        let existing: Option<(String, String, i64, Option<String>)> = tx
            .query_row(
                "SELECT operation,fingerprint,status,response FROM request_keys
                 WHERE operator=?1 AND key=?2",
                params![operator, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(internal)?;
        let result = match existing {
            Some((op, fp, status, response))
                if op == operation && fp == fingerprint && status == 1 =>
            {
                RequestKeyReservation::Replayed(response.unwrap_or_else(|| "null".to_owned()))
            }
            Some(_) => return Err(StoreError::Conflict { length: None }),
            None => {
                tx.execute(
                    "INSERT INTO request_keys(operator,key,operation,fingerprint,status,response,created_at,expires_at)
                     VALUES (?1,?2,?3,?4,0,NULL,?5,?6)",
                    params![
                        operator,
                        key,
                        operation,
                        fingerprint,
                        now,
                        now.saturating_add(i64::try_from(ttl_secs).unwrap_or(i64::MAX))
                    ],
                )
                .map_err(internal)?;
                RequestKeyReservation::Reserved
            }
        };
        tx.commit().map_err(internal)?;
        Ok(result)
    }

    fn complete_request_key(
        &self,
        operator: &str,
        key: &str,
        response: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let body = serde_json::to_string(response).map_err(internal)?;
        self.conn
            .lock()
            .map_err(|_| internal("mutex poisoned"))?
            .execute(
                "UPDATE request_keys SET status=1, response=?3
                 WHERE operator=?1 AND key=?2",
                params![operator, key, body],
            )
            .map_err(internal)?;
        Ok(())
    }

    fn release_request_key(&self, operator: &str, key: &str) -> Result<(), StoreError> {
        self.conn
            .lock()
            .map_err(|_| internal("mutex poisoned"))?
            .execute(
                "DELETE FROM request_keys WHERE operator=?1 AND key=?2",
                params![operator, key],
            )
            .map_err(internal)?;
        Ok(())
    }

    fn create_job(&self, spec: &JobSpec, now: Now) -> Result<(JobRecord, bool), StoreError> {
        self.create_job_inner(spec, now, None, 0, 0, false)
    }

    fn create_job_if_absent(
        &self,
        spec: &JobSpec,
        now: Now,
    ) -> Result<(JobRecord, bool), StoreError> {
        self.create_job_inner(spec, now, None, 0, 0, true)
    }

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
        let origin = spec.origin.clone().unwrap_or_default();
        let per_source: u64 = tx
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state='draft' AND COALESCE(json_extract(spec_json, '$.origin'), '') = ?1",
                params![origin],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c.max(0) as u64)
            .map_err(internal)?;
        let per_source_overflow = per_source.saturating_add(1).saturating_sub(max_per_source);
        if per_source_overflow > 0 {
            expire_drafts_in_tx(&tx, per_source_overflow, now, Some(&origin))?;
        }
        let total: u64 = tx
            .query_row("SELECT COUNT(*) FROM jobs WHERE state='draft'", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|c| c.max(0) as u64)
            .map_err(internal)?;
        let total_overflow = total.saturating_add(1).saturating_sub(max_total);
        if total_overflow > 0 {
            expire_drafts_in_tx(&tx, total_overflow, now, None)?;
        }
        let per_source: u64 = tx
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state='draft' AND COALESCE(json_extract(spec_json, '$.origin'), '') = ?1",
                params![origin],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c.max(0) as u64)
            .map_err(internal)?;
        let total: u64 = tx
            .query_row("SELECT COUNT(*) FROM jobs WHERE state='draft'", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|c| c.max(0) as u64)
            .map_err(internal)?;
        if total >= max_total || per_source >= max_per_source {
            return Err(StoreError::LimitExceeded("max_drafts"));
        }
        let rec = insert_job_tx(&tx, spec, now, None)?;
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
            false,
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
            .query_map(params![parent_job_id.as_str()], row_to_job_tuple)
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
            .query_row(params![id.as_str()], row_to_job_tuple)
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
                    let t: JobTuple = row_to_job_tuple(row)?;
                    Ok((t, row.get(30)?))
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
                row_to_job_tuple,
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
                    .query_map(params![limit as i64], row_to_job_tuple)
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
                        row_to_job_tuple(row)
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
                        row_to_job_tuple(row)
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
                        row_to_job_tuple(row)
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
        let (rec, _) = cancel_job_tx(&tx, id, now)?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn record_staged_commit(
        &self,
        id: &JobId,
        epoch: u64,
        commit: &str,
        now: Now,
    ) -> Result<JobRecord, StoreError> {
        if !valid_commit(commit) {
            return Err(StoreError::InvalidSpec(
                "staged commit must be exactly 40 lowercase hex digits".to_owned(),
            ));
        }
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        if rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        if !matches!(rec.state, JobState::Leased | JobState::Running) {
            return Err(StoreError::Conflict { length: None });
        }
        rec.staged_commit = Some(commit.to_owned());
        rec.updated_at = now.wall_unix;
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn blocked_dependents(&self, dep: &JobId) -> Result<Vec<JobRecord>, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {JOB_SELECT_COLUMNS} FROM jobs j
                 JOIN job_deps d ON d.job=j.id
                 WHERE d.dep=?1 AND j.state='blocked'
                   AND j.blocked_reason='predecessor failed'
                 ORDER BY j.id"
            ))
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![dep.as_str()], row_to_job_tuple)
            .map_err(internal)?;
        let mut out = Vec::new();
        for row in rows {
            let mut rec = tuple_to_job(row.map_err(internal)?)?;
            rec.waiting_on = load_waiting(&conn, rec.id.as_str())?;
            out.push(rec);
        }
        Ok(out)
    }

    fn expire_blocked_dependents(&self, now: Now, ttl_secs: u64) -> Result<Vec<JobId>, StoreError> {
        let ttl = ttl_secs.min(i64::MAX as u64) as i64;
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id FROM jobs WHERE state='blocked'
                       AND blocked_reason='predecessor failed'
                       AND blocked_since IS NOT NULL
                       AND blocked_since + ?1 <= ?2 ORDER BY id",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![ttl, now.wall_unix], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            let mut v = Vec::new();
            for row in rows {
                v.push(row.map_err(internal)?);
            }
            v
        };
        let mut moved = Vec::new();
        for id in ids {
            let (mut rec, _, _) = get_job_tx(&tx, &id)?;
            check_transition(rec.state, JobState::Failed)?;
            let predecessor = blocked_predecessor(&rec)
                .map(str::to_owned)
                .unwrap_or_else(|| "unknown predecessor".to_owned());
            rec.state = JobState::Failed;
            rec.failure_class = Some(FailureClass::Dependency);
            rec.failure_reason = None;
            rec.summary = Some(format!(
                "dependency {predecessor} failed; retry TTL {ttl_secs}s expired"
            ));
            rec.finished_at = Some(now.wall_unix);
            rec.updated_at = now.wall_unix;
            rec.blocked_reason = None;
            rec.blocked_since = None;
            rec.cancel_requested = false;
            update_job_tx(&tx, &rec, None, None)?;
            moved.push(rec.id);
        }
        tx.commit().map_err(internal)?;
        Ok(moved)
    }

    fn cancel_job_with_dependents(
        &self,
        id: &JobId,
        now: Now,
        keep_dependents: bool,
        expected_updated_at: Option<i64>,
    ) -> Result<JobRecord, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        check_expected_tx(&tx, id.as_str(), expected_updated_at)?;
        let (mut rec, cancelled_now) = cancel_job_tx(&tx, id, now)?;
        if cancelled_now && !keep_dependents {
            cancel_blocked_dependents_tx(&tx, id, now)?;
        }
        rec.waiting_on = load_waiting(&tx, rec.id.as_str())?;
        tx.commit().map_err(internal)?;
        Ok(rec)
    }

    fn retry_with_dependents(
        &self,
        id: &JobId,
        spec: &JobSpec,
        chain: bool,
        now: Now,
        expected_updated_at: Option<i64>,
    ) -> Result<(JobRecord, Vec<JobId>), StoreError> {
        spec.validate()
            .map_err(|e| StoreError::InvalidSpec(e.to_string()))?;
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        check_expected_tx(&tx, id.as_str(), expected_updated_at)?;
        // The root is written first so dependents only ever see the requeued predecessor.
        let root = requeue_retry_root_tx(&tx, id, spec, now)?;
        let requeued = restore_retry_dependents_tx(&tx, id, chain, now)?;
        tx.commit().map_err(internal)?;
        Ok((root, requeued))
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
                "SELECT {JOB_SELECT_COLUMNS} FROM jobs WHERE state='draft' ORDER BY created_at ASC, rowid ASC LIMIT ?1"
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

    fn approve_draft_if(
        &self,
        id: &JobId,
        edit: Option<&lu_proto::JobEdit>,
        target: JobState,
        now: Now,
        expected_updated_at: Option<i64>,
    ) -> Result<JobRecord, StoreError> {
        // The guard and the approval transition are checked in the same store transaction when
        // there is no edit. An edit writes first, so its check-then-write window is documented in
        // NOTES.md rather than hidden.
        {
            let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            check_expected_tx(&tx, id.as_str(), expected_updated_at)?;
            tx.commit().map_err(internal)?;
        }
        let rec = self.get_job(id)?;
        if rec.state != JobState::Draft {
            return Err(StoreError::Conflict { length: None });
        }
        let _rec = match edit {
            Some(edit) => self.edit_job(id, edit, now)?,
            None => rec,
        };
        self.approve_draft(id, target, now)
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
        // Clocks on a draft do not start until approval; this is the eligibility stamp.
        rec.eligible_since = now.wall_unix;
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

    fn expire_oldest_drafts(
        &self,
        count: u64,
        now: Now,
        origin: Option<&str>,
    ) -> Result<u64, StoreError> {
        if count == 0 {
            return Ok(0);
        }
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let ids: Vec<String> = {
            let limit = count.min(i64::MAX as u64) as i64;
            if let Some(origin) = origin {
                let mut stmt = tx
                    .prepare(
                        "SELECT id FROM jobs WHERE state='draft' AND COALESCE(json_extract(spec_json, '$.origin'), '') = ?2 ORDER BY created_at, rowid LIMIT ?1",
                    )
                    .map_err(internal)?;
                let rows = stmt
                    .query_map(params![limit, origin], |r| r.get::<_, String>(0))
                    .map_err(internal)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(internal)?
            } else {
                let mut stmt = tx
                    .prepare(
                        "SELECT id FROM jobs WHERE state='draft' ORDER BY created_at, rowid LIMIT ?1",
                    )
                    .map_err(internal)?;
                let rows = stmt
                    .query_map(params![limit], |r| r.get::<_, String>(0))
                    .map_err(internal)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(internal)?
            }
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
        refresh_health_cordons(&tx, hb)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn next_assignment(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
    ) -> Result<Option<Assignment>, StoreError> {
        self.next_assignment_with_limits(node, now, lease_ms, crate::DispatchLimits::default())
    }

    fn next_assignment_with_limits(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
        limits: crate::DispatchLimits,
    ) -> Result<Option<Assignment>, StoreError> {
        self.next_assignment_scheduled_with_limits(
            node,
            now,
            lease_ms,
            &lu_schedule::Schedule::default(),
            limits,
        )
    }

    fn next_assignment_scheduled(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
        schedule: &lu_schedule::Schedule,
    ) -> Result<Option<Assignment>, StoreError> {
        self.next_assignment_scheduled_with_limits(
            node,
            now,
            lease_ms,
            schedule,
            crate::DispatchLimits::default(),
        )
    }

    fn next_assignment_scheduled_with_limits(
        &self,
        node: &NodeName,
        now: Now,
        lease_ms: u64,
        schedule: &lu_schedule::Schedule,
        limits: crate::DispatchLimits,
    ) -> Result<Option<Assignment>, StoreError> {
        // Phase 1: a short read/write transaction collects the cheap checks and a snapshot of
        // candidates. Calendar evaluation happens with no write lock held. A second short
        // transaction re-checks the node and the chosen job before the lease is written, so a
        // concurrent poll cannot take the same job or the last group slot.
        let snapshot = match dispatch_snapshot(self, node, now, schedule, &limits)? {
            Some(snapshot) => snapshot,
            None => return Ok(None),
        };
        let poll = DispatchPoll {
            node,
            now,
            lease_ms,
            schedule,
            limits: &limits,
            snapshot: &snapshot,
        };
        for candidate in &snapshot.candidates {
            let job_schedule = candidate
                .spec
                .schedule
                .clone()
                .unwrap_or_else(lu_schedule::JobSchedule::default);
            for choice in &candidate.choices {
                let (provider, model) = choice;
                let request = lu_schedule::AdmissionRequest {
                    now: now.wall_unix,
                    eligible_since: candidate.eligible_since,
                    timeout_secs: candidate.spec.timeout_secs as i64,
                    job: job_schedule.clone(),
                    node: Some(node.as_str().to_owned()),
                    group: candidate.group.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                };
                match schedule.admit(&request) {
                    Ok(lu_schedule::Admission::Admitted(_)) => {
                        if let Some(assignment) = commit_dispatch(self, poll, candidate, choice)? {
                            return Ok(Some(assignment));
                        }
                        // The job or the node changed underneath us; this poll is done.
                        return Ok(None);
                    }
                    Ok(lu_schedule::Admission::Blocked(_)) => continue,
                    Err(error) => {
                        // A malformed job block must never lease the job. The submit path rejects
                        // these, but a hand-edited database must fail closed too.
                        let _ = error;
                        continue;
                    }
                }
            }
        }
        Ok(None)
    }

    /// Ends jobs that missed a start deadline with `on_deadline = expire|cancel`.
    fn expire_deadlines(
        &self,
        now: Now,
        _schedule: &lu_schedule::Schedule,
    ) -> Result<Vec<(JobId, lu_schedule::OnDeadline)>, StoreError> {
        use lu_schedule::OnDeadline;

        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let due = {
            let mut stmt = tx
                .prepare("SELECT id,spec_json FROM jobs WHERE state IN ('queued','blocked')")
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(internal)?;
            let mut due = Vec::new();
            for row in rows {
                let (id, spec_json) = row.map_err(internal)?;
                let spec: JobSpec = serde_json::from_str(&spec_json).map_err(internal)?;
                let Some(job) = &spec.schedule else { continue };
                let Some(deadline) = job.deadline else {
                    continue;
                };
                if now.wall_unix <= deadline || job.on_deadline == OnDeadline::Run {
                    continue;
                }
                due.push((id, job.on_deadline));
            }
            due
        };
        let mut moved = Vec::new();
        for (id, action) in due {
            let (mut rec, lease, recov) = get_job_tx(&tx, &id)?;
            if !matches!(rec.state, JobState::Queued | JobState::Blocked) {
                continue;
            }
            check_transition(rec.state, JobState::DeadlineMissed)?;
            rec.state = JobState::DeadlineMissed;
            rec.finished_at = Some(now.wall_unix);
            rec.updated_at = now.wall_unix;
            rec.summary = Some("deadline missed".to_owned());
            update_job_tx(&tx, &rec, lease, recov)?;
            moved.push((rec.id, action));
        }
        tx.commit().map_err(internal)?;
        Ok(moved)
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
        if !matches!(
            rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
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
        if !matches!(
            rec.state,
            JobState::Leased | JobState::Running | JobState::Recovering
        ) {
            return Err(StoreError::Conflict { length: None });
        }
        rec.scrubbed_hits = rec.scrubbed_hits.saturating_add(hits);
        rec.patch_scrubbed |= patch;
        update_job_tx(&tx, &rec, lease, recov)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn record_patch_secret_hits(
        &self,
        id: &JobId,
        epoch: u64,
        hits: &lu_proto::view::PatchSecretHitsView,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let (mut rec, lease, recov) = get_job_tx(&tx, id.as_str())?;
        if rec.epoch != epoch {
            return Err(StoreError::StaleEpoch);
        }
        rec.patch_secret_hits = hits.clone();
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
        node_attempts: u32,
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
                rec.failure_reason = None;
                update_job_tx(&tx, &rec, None, None)?;
                release_dependents(&tx, rec.id.as_str(), now)?;
            }
            Outcome::Cancelled => {
                check_transition(rec.state, JobState::Cancelled)?;
                rec.state = JobState::Cancelled;
                rec.finished_at = Some(now.wall_unix);
                rec.updated_at = now.wall_unix;
                rec.failure_reason = None;
                update_job_tx(&tx, &rec, None, None)?;
                fail_dependents(&tx, rec.id.as_str(), JobState::Cancelled.as_str(), now)?;
            }
            Outcome::Failed(out_class) => {
                let (class, provider) = failure.unwrap_or((out_class, None));
                rec.failure_class = Some(class);
                rec.failure_reason = if class == FailureClass::Node {
                    completion.failure_reason
                } else {
                    None
                };
                let has_diff =
                    rec.artifacts.contains(&ArtifactName::DiffPatch) || rec.diff_bytes.is_some();
                let retryable = if has_diff || rec.cancel_requested {
                    false
                } else if class == FailureClass::Node {
                    node_retry_allowed(&rec, max_retries, node_attempts)
                } else {
                    class.retryable(rec.attempt, max_retries)
                };
                if class == FailureClass::Node {
                    exclude_node(&mut rec, node_for_cordon.as_ref());
                }
                if retryable {
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
                                CordonSource::Health
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
                                    if source == CordonSource::Health { 1i64 } else { 0i64 },
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
        node_attempts: u32,
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
            let lost_node = rec.node.clone();
            let retryable = node_retry_allowed(&rec, max_retries, node_attempts);
            exclude_node(&mut rec, lost_node.as_ref());
            if retryable {
                check_transition(rec.state, JobState::Queued)?;
                rec.state = JobState::Queued;
                rec.attempt = rec.attempt.saturating_add(1);
                rec.node = None;
                rec.assigned_provider = None;
                rec.assigned_model = None;
                rec.failure_class = None;
                rec.failure_reason = None;
                rec.cancel_requested = false;
                rec.updated_at = now.wall_unix;
                rec.finished_at = None;
                update_job_tx(&tx, &rec, None, None)?;
            } else {
                check_transition(rec.state, JobState::Failed)?;
                rec.state = JobState::Failed;
                rec.failure_class = Some(FailureClass::Node);
                rec.failure_reason = None;
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
        // hours away against a fresh clock. Provider health and new failures set them again.
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
        let origin = if draining {
            crate::DrainOrigin::Operator
        } else {
            crate::DrainOrigin::None
        };
        tx.execute(
            "UPDATE nodes SET draining=?2 WHERE name=?1",
            params![node.as_str(), origin.as_raw()],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn set_agent_drain(&self, node: &NodeName) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let raw: Option<i64> = tx
            .query_row(
                "SELECT draining FROM nodes WHERE name=?1",
                params![node.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        if raw.is_none() {
            return Err(StoreError::NotFound);
        }
        // An operator drain is not the agent's to replace. Repeating an agent drain is a no-op.
        if !matches!(
            crate::DrainOrigin::from_raw(raw.unwrap_or(0)),
            crate::DrainOrigin::Operator
        ) {
            tx.execute(
                "UPDATE nodes SET draining=?2 WHERE name=?1",
                params![node.as_str(), crate::DrainOrigin::Agent.as_raw()],
            )
            .map_err(internal)?;
        }
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn clear_agent_drain(&self, node: &NodeName) -> Result<bool, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let raw: Option<i64> = tx
            .query_row(
                "SELECT draining FROM nodes WHERE name=?1",
                params![node.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        let raw = match raw {
            Some(raw) => raw,
            None => return Err(StoreError::NotFound),
        };
        match crate::DrainOrigin::from_raw(raw) {
            crate::DrainOrigin::None => {
                tx.commit().map_err(internal)?;
                Ok(false)
            }
            crate::DrainOrigin::Operator => Err(StoreError::Conflict { length: None }),
            crate::DrainOrigin::Agent => {
                tx.execute(
                    "UPDATE nodes SET draining=?2 WHERE name=?1",
                    params![node.as_str(), crate::DrainOrigin::None.as_raw()],
                )
                .map_err(internal)?;
                tx.commit().map_err(internal)?;
                Ok(true)
            }
        }
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
            let (name, hb_json, last_seen_wall, last_seen_mono, draining_raw) =
                r.map_err(internal)?;
            let drain_origin = crate::DrainOrigin::from_raw(draining_raw);
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
                draining: drain_origin.is_draining(),
                drain_origin,
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
                if source == CordonSource::Health { 1i64 } else { 0i64 },
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
            waiting_dependents: 0,
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
            .query_row("SELECT COUNT(*) FROM nodes WHERE draining<>0", [], |r| {
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
        let waiting: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state='blocked' AND blocked_reason='predecessor failed'",
                [],
                |r| r.get(0),
            )
            .map_err(internal)?;
        job_counts.waiting_dependents = waiting.max(0) as u64;
        Ok(job_counts)
    }

    fn stats(&self, query: &StatsQuery) -> Result<StatsReport, StoreError> {
        let conn = self.conn.lock().map_err(|_| internal("mutex poisoned"))?;
        let mut stmt = conn
            .prepare(
                "SELECT state, spec_json, attempt, node, created_at, started_at, finished_at,
                        failure_class
                 FROM jobs
                 WHERE state IN ('done','failed','cancelled','expired')
                   AND (?1 IS NULL OR finished_at >= ?1)
                   AND (?2 IS NULL OR node = ?2)",
            )
            .map_err(internal)?;
        let node_filter = query.node.as_ref().map(NodeName::as_str);
        let rows = stmt
            .query_map(params![query.since, node_filter], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })
            .map_err(internal)?;

        let mut fleet = StatsAccumulator::new();
        let mut per_node: BTreeMap<NodeName, StatsAccumulator> = BTreeMap::new();
        let mut observed_nodes = BTreeSet::new();
        for row in rows {
            let (
                state_text,
                spec_json,
                attempt,
                node_text,
                created_at,
                started_at,
                finished_at,
                failure_text,
            ) = row.map_err(internal)?;
            let state = parse_state(&state_text)?;
            let node = node_text
                .as_deref()
                .map(NodeName::parse)
                .transpose()
                .map_err(internal)?;
            let failure = failure_text.as_deref().map(parse_failure).transpose()?;
            let repo = normalize_repository(&decode_spec(&spec_json)?.repo);
            let job = StatsJob {
                state,
                failure,
                attempt: attempt.max(0) as u32,
                created_at,
                started_at,
                finished_at,
                repository: repo.as_deref(),
            };
            fleet.add_job(job);
            if let Some(node) = &node {
                observed_nodes.insert(node.clone());
                per_node
                    .entry(node.clone())
                    .or_insert_with(StatsAccumulator::new)
                    .add_job(job);
            }
        }

        let mut node_names: BTreeSet<NodeName> = if query.nodes.is_empty() {
            observed_nodes
        } else {
            query.nodes.iter().cloned().collect()
        };
        if let Some(node) = &query.node {
            node_names.retain(|name| name == node);
            node_names.insert(node.clone());
        }
        let nodes = node_names
            .into_iter()
            .map(|node| {
                let stats = per_node
                    .remove(&node)
                    .unwrap_or_else(StatsAccumulator::new)
                    .finish();
                NodeContribution { node, stats }
            })
            .collect();

        Ok(StatsReport {
            by: query.by,
            since: query.since,
            node: query.node.clone(),
            fleet: fleet.finish(),
            nodes,
        })
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
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    i64,
    Option<String>,
    String,
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
        row.get(26)?,
        row.get(27)?,
        row.get(28)?,
        row.get(29)?,
        row.get(30)?,
        row.get(31)?,
        row.get(32)?,
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
        patch_secret_hits_json,
        parent_job_id,
        root_job_id,
        depth,
        publish_json,
        staged_commit,
        blocked_reason,
        blocked_since,
        eligible_since,
        failure_reason_s,
        excluded_nodes_json,
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
        eligible_since,
        started_at,
        finished_at,
        failure_class: match failure_s {
            Some(f) => Some(parse_failure(&f)?),
            None => None,
        },
        failure_reason: parse_failure_reason(failure_reason_s)?,
        excluded_nodes: decode_excluded_nodes(&excluded_nodes_json)?,
        summary,
        exit_code,
        artifacts: decode_artifacts(&artifacts_json)?,
        cancel_requested: cancel_i != 0,
        log_len: log_len_i as u64,
        diff_bytes: diff_bytes.map(|v| v as u64),
        expect_failed: expect_failed != 0,
        scrubbed_hits: scrubbed_hits as u64,
        patch_scrubbed: patch_scrubbed != 0,
        patch_secret_hits: serde_json::from_str(&patch_secret_hits_json).unwrap_or_default(),
        waiting_on: Vec::new(),
        staged_commit,
        blocked_reason,
        blocked_since,
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
    use lu_proto::{FailureClass, JobState, NodeName, StatsQuery};
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

    struct TerminalRow<'a> {
        key: &'a str,
        repo: &'a str,
        node: &'a str,
        state: JobState,
        failure: Option<FailureClass>,
        attempt: u32,
        created: i64,
        started: Option<i64>,
        finished: Option<i64>,
    }

    fn insert_terminal(store: &SqliteStore, row: TerminalRow<'_>) {
        let TerminalRow {
            key,
            repo,
            node,
            state,
            failure,
            attempt,
            created,
            started,
            finished,
        } = row;
        // New specs may not carry credentials, but stored rows from before that rule can: create
        // the job with a clean URL, then put the original text back to exercise normalisation.
        let clean_repo = match repo.split_once("://").and_then(|(scheme, rest)| {
            rest.split_once('@')
                .map(|(_, host)| format!("{scheme}://{host}"))
        }) {
            Some(clean) => clean,
            None => repo.to_string(),
        };
        let spec = crate::conformance::spec(&clean_repo, Some(key));
        let (job, _) = store
            .create_job(
                &spec,
                Now {
                    wall_unix: created,
                    mono_ms: created.max(0) as u64,
                },
            )
            .unwrap();
        if clean_repo != repo {
            store
                .conn
                .lock()
                .unwrap()
                .execute(
                    "UPDATE jobs SET spec_json=replace(spec_json, ?2, ?3) WHERE id=?1",
                    params![job.id.as_str(), clean_repo, repo],
                )
                .unwrap();
        }
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE jobs SET state=?2,node=?3,attempt=?4,started_at=?5,finished_at=?6,
                               failure_class=?7 WHERE id=?1",
                params![
                    job.id.as_str(),
                    state.as_str(),
                    node,
                    attempt as i64,
                    started,
                    finished,
                    failure.map(FailureClass::as_str),
                ],
            )
            .unwrap();
    }

    fn configured_nodes() -> Vec<NodeName> {
        ["node-a", "node-b", "node-c"]
            .into_iter()
            .map(|name| name.parse().unwrap())
            .collect()
    }

    #[test]
    fn conformance() {
        crate::conformance::run_all(|| {
            let p = temp_db("db.sqlite");
            SqliteStore::open(&p).unwrap()
        });
    }

    #[test]
    fn stats_windows_nodes_retries_and_missing_times() {
        let store = SqliteStore::open(temp_db("stats.sqlite")).unwrap();
        insert_terminal(
            &store,
            TerminalRow {
                key: "old",
                repo: "https://git.example.test/team/old.git",
                node: "node-a",
                state: JobState::Done,
                failure: None,
                attempt: 1,
                created: 0,
                started: Some(0),
                finished: Some(50),
            },
        );
        let credential = ["cr", "ed"].join("");
        let credentialed_repo = format!("https://user:{credential}@git.example.test/team/app.git");
        insert_terminal(
            &store,
            TerminalRow {
                key: "done",
                repo: &credentialed_repo,
                node: "node-a",
                state: JobState::Done,
                failure: None,
                attempt: 2,
                created: 100,
                started: Some(110),
                finished: Some(130),
            },
        );
        insert_terminal(
            &store,
            TerminalRow {
                key: "failed",
                repo: "https://git.example.test/team/app",
                node: "node-a",
                state: JobState::Failed,
                failure: Some(FailureClass::Provider),
                attempt: 1,
                created: 200,
                started: Some(210),
                finished: Some(270),
            },
        );
        insert_terminal(
            &store,
            TerminalRow {
                key: "cancelled",
                repo: "https://git.example.test/other/lib.git",
                node: "node-b",
                state: JobState::Cancelled,
                failure: None,
                attempt: 1,
                created: 300,
                started: Some(320),
                finished: Some(325),
            },
        );
        insert_terminal(
            &store,
            TerminalRow {
                key: "expired",
                repo: "https://git.example.test/other/lib",
                node: "node-b",
                state: JobState::Expired,
                failure: None,
                attempt: 1,
                created: 400,
                started: None,
                finished: Some(460),
            },
        );
        insert_terminal(
            &store,
            TerminalRow {
                key: "unfinished",
                repo: "https://git.example.test/other/lib",
                node: "node-b",
                state: JobState::Cancelled,
                failure: None,
                attempt: 1,
                created: 500,
                started: Some(510),
                finished: None,
            },
        );
        let (queued, _) = store
            .create_job(
                &crate::conformance::spec(
                    "https://git.example.test/team/queued.git",
                    Some("queued"),
                ),
                Now {
                    wall_unix: 600,
                    mono_ms: 600,
                },
            )
            .unwrap();
        assert_eq!(queued.state, JobState::Queued);

        let window = store
            .stats(&StatsQuery {
                since: Some(100),
                nodes: configured_nodes(),
                ..StatsQuery::default()
            })
            .unwrap();
        assert_eq!(window.since, Some(100));
        assert_eq!(window.fleet.states["done"], 1);
        assert_eq!(window.fleet.states["failed"], 1);
        assert_eq!(window.fleet.states["cancelled"], 1);
        assert_eq!(window.fleet.states["expired"], 1);
        assert_eq!(window.fleet.failures["provider"], 1);
        assert_eq!(window.fleet.retries, 1);
        assert_eq!(window.fleet.busy_seconds, 145);
        assert_eq!(window.fleet.last_finished_at, Some(460));
        assert_eq!(window.fleet.runtime.count, 4);
        assert_eq!(window.fleet.runtime.sum_seconds, 145);
        assert_eq!(window.fleet.runtime.median_seconds, Some(20));
        assert_eq!(window.fleet.runtime.p90_seconds, Some(60));
        assert_eq!(window.fleet.runtime.buckets["5"], 1);
        assert_eq!(window.fleet.runtime.buckets["60"], 4);
        assert_eq!(window.fleet.runtime.buckets["+Inf"], 4);
        assert_eq!(window.fleet.queue_wait.count, 3);
        assert_eq!(window.fleet.queue_wait.median_seconds, Some(10));
        assert_eq!(window.fleet.queue_wait.p90_seconds, Some(20));
        assert_eq!(
            window.fleet.top_repositories,
            vec![
                lu_proto::RepoContribution {
                    repository: "git.example.test/other/lib".to_owned(),
                    jobs: 2,
                },
                lu_proto::RepoContribution {
                    repository: "git.example.test/team/app".to_owned(),
                    jobs: 2,
                },
            ]
        );

        assert_eq!(window.nodes.len(), 3);
        let node_a = window
            .nodes
            .iter()
            .find(|row| row.node.as_str() == "node-a")
            .unwrap();
        assert_eq!(node_a.stats.states["done"], 1);
        assert_eq!(node_a.stats.failures["provider"], 1);
        assert_eq!(node_a.stats.retries, 1);
        assert_eq!(node_a.stats.busy_seconds, 80);
        let node_b = window
            .nodes
            .iter()
            .find(|row| row.node.as_str() == "node-b")
            .unwrap();
        assert_eq!(node_b.stats.states["cancelled"], 1);
        assert_eq!(node_b.stats.states["expired"], 1);
        assert_eq!(node_b.stats.busy_seconds, 65);
        let node_c = window
            .nodes
            .iter()
            .find(|row| row.node.as_str() == "node-c")
            .unwrap();
        assert_eq!(node_c.stats.busy_seconds, 0);
        assert_eq!(node_c.stats.runtime.count, 0);
        assert_eq!(node_c.stats.last_finished_at, None);

        let all = store
            .stats(&StatsQuery {
                nodes: configured_nodes(),
                ..StatsQuery::default()
            })
            .unwrap();
        assert_eq!(all.fleet.states["done"], 2);
        assert_eq!(all.fleet.states["cancelled"], 2);
        assert_eq!(all.fleet.busy_seconds, 195);
        assert_eq!(all.fleet.runtime.count, 5);
        assert_eq!(all.fleet.queue_wait.count, 5);
        assert_eq!(all.fleet.last_finished_at, Some(460));

        let only_b = store
            .stats(&StatsQuery {
                node: Some("node-b".parse().unwrap()),
                nodes: configured_nodes(),
                ..StatsQuery::default()
            })
            .unwrap();
        assert_eq!(only_b.node.as_ref().map(NodeName::as_str), Some("node-b"));
        assert_eq!(only_b.nodes.len(), 1);
        assert_eq!(only_b.fleet.states["cancelled"], 2);
        assert_eq!(only_b.fleet.states["done"], 0);
        assert_eq!(only_b.fleet.busy_seconds, 65);
    }

    #[test]
    fn stats_top_repositories_are_capped_and_normalised() {
        let store = SqliteStore::open(temp_db("stats-top.sqlite")).unwrap();
        for index in 0..12 {
            let repo = format!("https://git.example.test/owner/repo{index}.git");
            insert_terminal(
                &store,
                TerminalRow {
                    key: &format!("repo-{index}"),
                    repo: &repo,
                    node: "node-a",
                    state: JobState::Done,
                    failure: None,
                    attempt: 1,
                    created: index * 10,
                    started: Some(index * 10),
                    finished: Some(index * 10 + 5),
                },
            );
        }
        let report = store
            .stats(&StatsQuery {
                nodes: configured_nodes(),
                ..StatsQuery::default()
            })
            .unwrap();
        assert_eq!(report.fleet.top_repositories.len(), 10);
        assert_eq!(
            report.fleet.top_repositories[0].repository,
            "git.example.test/owner/repo0"
        );
        assert_eq!(
            report.fleet.top_repositories[9].repository,
            "git.example.test/owner/repo7"
        );
        assert!(report
            .fleet
            .top_repositories
            .iter()
            .all(|row| !row.repository.contains("://") && !row.repository.ends_with(".git")));
    }

    #[test]
    fn stats_survive_restart_and_windows_never_raise_all_time_counters() {
        let path = temp_db("stats-restart.sqlite");
        {
            let store = SqliteStore::open(&path).unwrap();
            insert_terminal(
                &store,
                TerminalRow {
                    key: "old",
                    repo: "https://git.example.test/team/old.git",
                    node: "node-a",
                    state: JobState::Done,
                    failure: None,
                    attempt: 1,
                    created: 0,
                    started: Some(0),
                    finished: Some(20),
                },
            );
            insert_terminal(
                &store,
                TerminalRow {
                    key: "new",
                    repo: "https://git.example.test/team/new.git",
                    node: "node-a",
                    state: JobState::Failed,
                    failure: Some(FailureClass::Execution),
                    attempt: 3,
                    created: 100,
                    started: Some(110),
                    finished: Some(170),
                },
            );
        }
        let reopened = SqliteStore::open(&path).unwrap();
        let all_time = reopened
            .stats(&StatsQuery {
                nodes: configured_nodes(),
                ..StatsQuery::default()
            })
            .unwrap();
        assert_eq!(all_time.fleet.states["done"], 1);
        assert_eq!(all_time.fleet.states["failed"], 1);
        assert_eq!(all_time.fleet.retries, 2);
        assert_eq!(all_time.fleet.busy_seconds, 80);

        let window = reopened
            .stats(&StatsQuery {
                since: Some(100),
                nodes: configured_nodes(),
                ..StatsQuery::default()
            })
            .unwrap();
        assert_eq!(window.fleet.states["done"], 0);
        assert_eq!(window.fleet.states["failed"], 1);
        assert!(all_time.fleet.busy_seconds >= window.fleet.busy_seconds);

        // Re-reading the same persistent source after another open gives the same counters.
        let second_open = SqliteStore::open(&path).unwrap();
        let again = second_open
            .stats(&StatsQuery {
                nodes: configured_nodes(),
                ..StatsQuery::default()
            })
            .unwrap();
        assert_eq!(again, all_time);
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
            stack_union_paths: Vec::new(),
            stack_base_paths: Vec::new(),
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
            schedule: None,
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
        for version in [0i64, 9, 10, 999] {
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
    fn schema_1_database_upgrades_with_stack_blocking_columns() {
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
                stack_union_paths: Vec::new(),
                stack_base_paths: Vec::new(),
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
                schedule: None,
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
                 ALTER TABLE jobs DROP COLUMN staged_commit;
                 ALTER TABLE jobs DROP COLUMN blocked_reason;
                 ALTER TABLE jobs DROP COLUMN blocked_since;
                 UPDATE schema_version SET version = 1;",
            )
            .unwrap();
        }
        let s = SqliteStore::open(&p).unwrap();
        let conn = rusqlite::Connection::open(&p).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::SCHEMA_VERSION);
        let jobs = s.list_jobs(None, 10, None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].scrubbed_hits, 0);
        assert!(!jobs[0].patch_scrubbed);
        assert_eq!(jobs[0].patch_secret_hits.count, 0);
        assert!(jobs[0].patch_secret_hits.rules.is_empty());
        assert_eq!(jobs[0].parent_job_id, None);
        assert_eq!(jobs[0].root_job_id, None);
        assert_eq!(jobs[0].depth, 0);
        assert_eq!(jobs[0].staged_commit, None);
        assert_eq!(jobs[0].blocked_reason, None);
        assert_eq!(jobs[0].blocked_since, None);
        let n = crate::conformance::node("test-node");
        s.heartbeat(
            &crate::conformance::hb("test-node", lu_proto::HarnessState::Idle, vec![]),
            Now {
                wall_unix: 2,
                mono_ms: 2,
            },
        )
        .unwrap();
        let a = s
            .next_assignment(
                &n,
                Now {
                    wall_unix: 2,
                    mono_ms: 3,
                },
                60_000,
            )
            .unwrap()
            .unwrap();
        assert_eq!(a.job_id, jobs[0].id);
        s.record_scrub(&jobs[0].id, a.epoch, 3, true).unwrap();
        let job = s.get_job(&jobs[0].id).unwrap();
        assert_eq!(job.scrubbed_hits, 3);
        assert!(job.patch_scrubbed);
    }

    #[test]
    fn schema_3_database_gains_stack_blocking_columns() {
        let p = temp_db("schema3.sqlite");
        let now = Now {
            wall_unix: 1,
            mono_ms: 1,
        };
        {
            let s = SqliteStore::open(&p).unwrap();
            let (a, _) = s
                .create_job(&crate::conformance::spec("https://x/a.git", None), now)
                .unwrap();
            let mut dependent = crate::conformance::spec("https://x/b.git", None);
            dependent.after = vec![a.id.clone()];
            let (b, _) = s.create_job(&dependent, now).unwrap();
            assert_eq!(b.state, lu_proto::JobState::Blocked);
        }
        // Simulate a version-3 database: the three stacked-chain columns did not exist yet.
        {
            let conn = rusqlite::Connection::open(&p).unwrap();
            conn.execute_batch(
                "ALTER TABLE jobs DROP COLUMN staged_commit;
                 ALTER TABLE jobs DROP COLUMN blocked_reason;
                 ALTER TABLE jobs DROP COLUMN blocked_since;
                 UPDATE schema_version SET version = 3;",
            )
            .unwrap();
        }
        let s = SqliteStore::open(&p).unwrap();
        let conn = rusqlite::Connection::open(&p).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::SCHEMA_VERSION);
        let jobs = s.list_jobs(None, 10, None).unwrap();
        assert_eq!(jobs.len(), 2);
        for job in &jobs {
            assert_eq!(job.staged_commit, None);
            assert_eq!(job.blocked_reason, None);
            assert_eq!(job.blocked_since, None);
            assert_eq!(job.patch_secret_hits.count, 0);
            assert!(job.patch_secret_hits.rules.is_empty());
        }
        let blocked = jobs
            .iter()
            .find(|job| job.state == lu_proto::JobState::Blocked)
            .expect("the dependent was still blocked");
        assert_eq!(blocked.waiting_on.len(), 1);
        // Reopening a migrated database is a no-op.
        drop(s);
        drop(conn);
        let s = SqliteStore::open(&p).unwrap();
        assert_eq!(s.list_jobs(None, 10, None).unwrap().len(), 2);
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
                 VALUES('node-old','zen',70000000,0,0),('node-old','openrouter',90000000,1,0)",
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

    #[test]
    fn stored_specs_with_credentials_stay_readable_and_redacted() {
        let store = SqliteStore::open(temp_db("legacy-repo.sqlite")).unwrap();
        let spec = crate::conformance::spec("https://git.example.test/team/app.git", Some("k1"));
        let (job, _) = store
            .create_job(
                &spec,
                Now {
                    wall_unix: 10,
                    mono_ms: 10,
                },
            )
            .unwrap();
        let credential = ["pw", "legacy", "value"].join("-");
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE jobs SET spec_json=replace(spec_json, 'https://git.example.test/', ?2) WHERE id=?1",
                params![
                    job.id.as_str(),
                    format!("https://user:{credential}@git.example.test/")
                ],
            )
            .unwrap();
        let record = store.get_job(&job.id).unwrap();
        assert_eq!(record.spec.repo, "https://git.example.test/team/app.git");
        assert_eq!(
            record.spec.source.repo,
            "https://git.example.test/team/app.git"
        );
    }

    #[test]
    fn schema_five_database_gains_eligible_since_in_place() {
        let path = temp_db("v5.sqlite");
        drop(SqliteStore::open(&path).unwrap());
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE jobs DROP COLUMN eligible_since;
                 UPDATE schema_version SET version = 5;",
            )
            .unwrap();
        }
        drop(SqliteStore::open(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::SCHEMA_VERSION);
        let has_column: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name = 'eligible_since'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_column, 1);
    }

    #[test]
    fn request_keys_replay_only_matching_completed_requests() {
        let path = temp_db("request-keys.sqlite");
        let store = SqliteStore::open(&path).unwrap();
        let reservation = store
            .reserve_request_key("principal", "key-1", "create", "fp-1", 60)
            .unwrap();
        assert_eq!(reservation, crate::RequestKeyReservation::Reserved);
        store
            .complete_request_key("principal", "key-1", &serde_json::json!({"ok": true}))
            .unwrap();
        assert_eq!(
            store
                .reserve_request_key("principal", "key-1", "create", "fp-1", 60)
                .unwrap(),
            crate::RequestKeyReservation::Replayed("{\"ok\":true}".to_owned())
        );
        assert!(matches!(
            store.reserve_request_key("principal", "key-1", "create", "fp-2", 60),
            Err(crate::StoreError::Conflict { .. })
        ));
        assert!(matches!(
            store.reserve_request_key("principal", "key-1", "cancel", "fp-1", 60),
            Err(crate::StoreError::Conflict { .. })
        ));
        store.release_request_key("principal", "key-1").unwrap();
        assert_eq!(
            store
                .reserve_request_key("principal", "key-1", "cancel", "fp-1", 60)
                .unwrap(),
            crate::RequestKeyReservation::Reserved
        );
    }

    #[test]
    fn request_keys_expire() {
        let path = temp_db("request-keys-expire.sqlite");
        let store = SqliteStore::open(&path).unwrap();
        store
            .reserve_request_key("principal", "key-1", "create", "fp-1", 0)
            .unwrap();
        store
            .complete_request_key("principal", "key-1", &serde_json::json!({"ok": true}))
            .unwrap();
        assert_eq!(
            store
                .reserve_request_key("principal", "key-1", "create", "fp-1", 60)
                .unwrap(),
            crate::RequestKeyReservation::Reserved
        );
    }

    #[test]
    fn schema_six_database_gains_request_keys_in_place() {
        let path = temp_db("v6-request-keys.sqlite");
        let spec =
            crate::conformance::spec("https://x/v6-request-keys.git", Some("v6-request-keys"));
        {
            let store = SqliteStore::open(&path).unwrap();
            store
                .create_job(
                    &spec,
                    Now {
                        wall_unix: 1_700_000_000,
                        mono_ms: 1_000,
                    },
                )
                .unwrap();
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TABLE request_keys;
                 UPDATE schema_version SET version = 6;",
            )
            .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        assert!(store.job_by_key("v6-request-keys").is_some());
        let conn = store.conn.lock().unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::SCHEMA_VERSION);
        let table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='request_keys'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table, 1);
    }

    #[test]
    fn schema_seven_database_gains_node_failure_columns_in_place() {
        let path = temp_db("v7-node-failures.sqlite");
        let spec =
            crate::conformance::spec("https://x/v7-node-failures.git", Some("v7-node-failures"));
        {
            let store = SqliteStore::open(&path).unwrap();
            store
                .create_job(
                    &spec,
                    Now {
                        wall_unix: 1_700_000_000,
                        mono_ms: 1_000,
                    },
                )
                .unwrap();
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE jobs DROP COLUMN failure_reason;
                 ALTER TABLE jobs DROP COLUMN excluded_nodes_json;
                 UPDATE schema_version SET version = 7;",
            )
            .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let rec = store.job_by_key("v7-node-failures").unwrap();
        assert_eq!(rec.failure_reason, None);
        assert!(rec.excluded_nodes.is_empty());
        let conn = store.conn.lock().unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::SCHEMA_VERSION);
        let (reason, exclusions): (Option<String>, String) = conn
            .query_row(
                "SELECT failure_reason, excluded_nodes_json FROM jobs WHERE id = ?1",
                params![rec.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, None);
        assert_eq!(exclusions, "[]");
    }
}
