//! Online database backup, restore and read-only inspection helpers.
//!
//! These functions open their own SQLite connections so the controller's connection is not
//! involved; `VACUUM INTO` gives a consistent copy while the controller keeps serving.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::SCHEMA_VERSION;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupInfo {
    pub bytes: u64,
    pub schema_version: Option<i64>,
    pub jobs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreInfo {
    pub pre_restore: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseCheck {
    pub quick_check: String,
    pub schema_version: Option<i64>,
    pub bytes: u64,
    pub leased_without_lease: u64,
    pub orphan_job_deps: u64,
}

/// Copies `db` to `out` with `VACUUM INTO`. The destination is new unless `force` is set.
pub fn backup(db: &Path, out: &Path, force: bool) -> Result<BackupInfo, String> {
    if !db.exists() {
        return Err(format!("database {} does not exist", db.display()));
    }
    if db == out {
        return Err("the backup output must differ from the database".to_owned());
    }
    if out.exists() {
        if !force {
            return Err(format!("{} already exists (use --force)", out.display()));
        }
        fs::remove_file(out)
            .map_err(|error| format!("cannot replace {}: {error}", out.display()))?;
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!(
                "backup directory does not exist: {}",
                parent.display()
            ));
        }
    }
    let source = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
    source
        .busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|error| error.to_string())?;
    let out_text = out
        .to_str()
        .ok_or_else(|| format!("backup path is not valid UTF-8: {}", out.display()))?;
    source
        .execute("VACUUM INTO ?1", params![out_text])
        .map_err(|error| format!("cannot back up {}: {error}", db.display()))?;
    drop(source);
    set_private(out)?;
    verify_integrity(out, "integrity_check")?;
    read_info(out)
}

/// Installs `backup` at `db` after checking it and moving the current database aside.
pub fn restore(backup: &Path, db: &Path) -> Result<RestoreInfo, String> {
    if !backup.exists() {
        return Err(format!("backup {} does not exist", backup.display()));
    }
    verify_integrity(backup, "integrity_check")?;
    let info = read_info(backup)?;
    if let Some(version) = info.schema_version {
        if version > SCHEMA_VERSION {
            return Err(format!(
                "backup schema version {version} is newer than supported {SCHEMA_VERSION}"
            ));
        }
    }
    if let Some(parent) = db.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!(
                "database directory does not exist: {}",
                parent.display()
            ));
        }
    }

    let mut pre_restore = None;
    if db.exists() {
        // The advisory lock catches a second restore; the SQLite exclusive transaction catches a
        // live controller or another writer.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(db)
            .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
        lock.try_lock()
            .map_err(|_| format!("database {} is in use; stop the controller", db.display()))?;
        {
            let conn = Connection::open(db)
                .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
            conn.busy_timeout(std::time::Duration::ZERO)
                .map_err(|error| error.to_string())?;
            conn.execute_batch("BEGIN EXCLUSIVE; ROLLBACK;")
                .map_err(|_| format!("database {} is in use; stop the controller", db.display()))?;
            let busy: i64 = conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                .unwrap_or(0);
            if busy != 0 {
                return Err(format!(
                    "database {} is in use; stop the controller",
                    db.display()
                ));
            }
        }
        let moved = pre_restore_path(db);
        fs::rename(db, &moved)
            .map_err(|error| format!("cannot move {} aside: {error}", db.display()))?;
        for suffix in ["-wal", "-shm"] {
            let from = sidecar(db, suffix);
            if from.exists() {
                let to = sidecar(&moved, suffix);
                let _ = fs::rename(from, to);
            }
        }
        pre_restore = Some(moved);
    }

    let installed = fs::copy(backup, db).and_then(|_| {
        fs::set_permissions(db, fs::Permissions::from_mode(0o600))?;
        Ok(())
    });
    if let Err(error) = installed {
        if let Some(moved) = &pre_restore {
            let _ = fs::rename(moved, db);
        }
        return Err(format!("cannot install {}: {error}", db.display()));
    }
    Ok(RestoreInfo { pre_restore })
}

/// Reads integrity, schema and a few consistency counts without changing the database.
pub fn inspect(db: &Path) -> Result<DatabaseCheck, String> {
    if !db.exists() {
        return Err(format!("database {} does not exist", db.display()));
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
    let quick_check = check_text(&conn, "quick_check")?;
    let schema_version = schema_version(&conn)?;
    let leased_without_lease = if table_exists(&conn, "jobs")? {
        count(
            &conn,
            "SELECT COUNT(*) FROM jobs WHERE state IN ('leased','running') \
             AND lease_until_mono_ms IS NULL",
        )?
    } else {
        0
    };
    let orphan_job_deps = if table_exists(&conn, "job_deps")? {
        count(
            &conn,
            "SELECT COUNT(*) FROM job_deps d \
             WHERE NOT EXISTS (SELECT 1 FROM jobs j WHERE j.id=d.job) \
             OR NOT EXISTS (SELECT 1 FROM jobs j WHERE j.id=d.dep)",
        )?
    } else {
        0
    };
    Ok(DatabaseCheck {
        quick_check,
        schema_version,
        bytes: fs::metadata(db)
            .map_err(|error| format!("cannot stat {}: {error}", db.display()))?
            .len(),
        leased_without_lease,
        orphan_job_deps,
    })
}

fn read_info(db: &Path) -> Result<BackupInfo, String> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
    let schema_version = schema_version(&conn)?;
    let jobs = if table_exists(&conn, "jobs")? {
        count(&conn, "SELECT COUNT(*) FROM jobs")?
    } else {
        0
    };
    Ok(BackupInfo {
        bytes: fs::metadata(db)
            .map_err(|error| format!("cannot stat {}: {error}", db.display()))?
            .len(),
        schema_version,
        jobs,
    })
}

fn verify_integrity(path: &Path, pragma: &str) -> Result<(), String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let result = check_text(&conn, pragma)?;
    if result == "ok" {
        Ok(())
    } else {
        Err(format!(
            "integrity check failed for {}: {result}",
            path.display()
        ))
    }
}

fn check_text(conn: &Connection, pragma: &str) -> Result<String, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA {pragma}"))
        .map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("integrity check failed: {error}"))?;
    let mut lines = Vec::new();
    for row in rows {
        lines.push(row.map_err(|error| format!("integrity check failed: {error}"))?);
    }
    Ok(lines.join("; "))
}

fn schema_version(conn: &Connection) -> Result<Option<i64>, String> {
    if !table_exists(conn, "schema_version")? {
        return Ok(None);
    }
    conn.query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
        row.get(0)
    })
    .optional()
    .map_err(|error| error.to_string())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, String> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            params![name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    Ok(exists.is_some())
}

fn count(conn: &Connection, sql: &str) -> Result<u64, String> {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|value| value.max(0) as u64)
        .map_err(|error| error.to_string())
}

fn set_private(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot set the mode of {}: {error}", path.display()))
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn pre_restore_path(db: &Path) -> PathBuf {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let base = format!("{}.pre-restore-{seconds}", db.display());
    let mut candidate = PathBuf::from(&base);
    let mut counter = 1u32;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{base}-{counter}"));
        counter += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Now, Store};
    use lu_proto::{JobSpec, JobState};
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lu-backup-test-{}-{}-{name}",
            std::process::id(),
            CTR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec(n: u32) -> JobSpec {
        JobSpec {
            repo: format!("https://example.net/r{n}.git"),
            git_ref: "main".to_owned(),
            task: "t".to_owned(),
            harness: "opencode".to_owned(),
            provider: None,
            model: None,
            timeout_secs: 3600,
            idle_timeout_secs: None,
            idempotency_key: None,
            after: Vec::new(),
        }
    }

    fn now(mono: u64) -> Now {
        Now {
            wall_unix: 100 + mono as i64,
            mono_ms: mono,
        }
    }

    #[test]
    fn backs_up_while_a_writer_inserts() {
        let dir = temp_dir("live");
        let db = dir.join("lu.db");
        let store = std::sync::Arc::new(crate::sqlite::SqliteStore::open(&db).unwrap());
        for n in 0..5 {
            store.create_job(&spec(n), now(u64::from(n))).unwrap();
        }
        let writer = {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                for n in 5..25 {
                    store.create_job(&spec(n), now(u64::from(n))).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };
        let out = dir.join("backup.db");
        let info = backup(&db, &out, false).unwrap();
        writer.join().unwrap();
        assert!(info.jobs >= 5, "{info:?}");
        assert_eq!(info.schema_version, Some(SCHEMA_VERSION));
        let check = inspect(&out).unwrap();
        assert_eq!(check.quick_check, "ok");
        assert_eq!(check.schema_version, Some(SCHEMA_VERSION));
        assert_eq!(
            fs::metadata(&out).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_round_trips_and_boots_into_recovery() {
        let dir = temp_dir("restore");
        let original = dir.join("original.db");
        let target = dir.join("target.db");
        let backup_path = dir.join("copy.db");
        let store = crate::sqlite::SqliteStore::open(&original).unwrap();
        let (job, _) = store.create_job(&spec(1), now(1)).unwrap();
        let hb = crate::conformance::hb("node-a", lu_proto::HarnessState::Idle, Vec::new());
        store.heartbeat(&hb, now(1)).unwrap();
        let assignment = store.next_assignment(&hb.node, now(1), 60_000).unwrap();
        assert!(assignment.is_some());
        store.start_job(&job.id, 1, now(2)).unwrap();
        assert_eq!(store.get_job(&job.id).unwrap().state, JobState::Running);
        backup(&original, &backup_path, false).unwrap();

        let target_store = crate::sqlite::SqliteStore::open(&target).unwrap();
        target_store.create_job(&spec(99), now(1)).unwrap();
        drop(target_store);
        let restored = restore(&backup_path, &target).unwrap();
        assert!(restored.pre_restore.is_some());
        assert!(restored.pre_restore.as_ref().unwrap().exists());

        let reopened = crate::sqlite::SqliteStore::open(&target).unwrap();
        assert_eq!(reopened.get_job(&job.id).unwrap().spec.repo, spec(1).repo);
        assert_eq!(reopened.recover_on_boot(now(3)).unwrap(), 1);
        assert_eq!(
            reopened.get_job(&job.id).unwrap().state,
            JobState::Recovering
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = temp_dir("newer");
        let db = dir.join("lu.db");
        let out = dir.join("newer.db");
        {
            let _store = crate::sqlite::SqliteStore::open(&db).unwrap();
        }
        let conn = Connection::open(&db).unwrap();
        conn.execute("UPDATE schema_version SET version=999", [])
            .unwrap();
        drop(conn);
        backup(&db, &out, false).unwrap();
        let target = dir.join("target.db");
        let error = restore(&out, &target).unwrap_err();
        assert!(error.contains("newer"), "{error}");
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_overwrite_unless_forced() {
        let dir = temp_dir("overwrite");
        let db = dir.join("lu.db");
        let out = dir.join("copy.db");
        let _store = crate::sqlite::SqliteStore::open(&db).unwrap();
        backup(&db, &out, false).unwrap();
        let error = backup(&db, &out, false).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        let info = backup(&db, &out, true).unwrap();
        assert_eq!(info.schema_version, Some(SCHEMA_VERSION));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_refuses_a_live_database() {
        let dir = temp_dir("live-restore");
        let db = dir.join("lu.db");
        let copy = dir.join("copy.db");
        let store = crate::sqlite::SqliteStore::open(&db).unwrap();
        backup(&db, &copy, false).unwrap();
        let error = restore(&copy, &db).unwrap_err();
        assert!(error.contains("in use"), "{error}");
        drop(store);
        restore(&copy, &db).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_corrupt_file() {
        let dir = temp_dir("corrupt");
        let bad = dir.join("bad.db");
        fs::write(&bad, b"this is not a sqlite database").unwrap();
        let target = dir.join("target.db");
        let error = restore(&bad, &target).unwrap_err();
        assert!(
            error.contains("integrity") || error.contains("not a database"),
            "{error}"
        );
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
