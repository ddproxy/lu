//! Online database backup, restore and read-only inspection helpers.
//!
//! These functions open their own SQLite connections so the controller's connection is not
//! involved; `VACUUM INTO` gives a consistent copy while the controller keeps serving.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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

/// The live database and its sidecars, as paths a backup output must never be.
fn live_paths(db: &Path) -> Vec<PathBuf> {
    let base = fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
    vec![
        base.clone(),
        sidecar(&base, "-wal"),
        sidecar(&base, "-shm"),
        sidecar(&base, "-journal"),
    ]
}

/// Resolves `out` through its directory so `..` and symlinked directories cannot hide that it
/// is the live database.
fn resolved(out: &Path) -> PathBuf {
    let name = out.file_name().map(PathBuf::from).unwrap_or_default();
    let dir = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::canonicalize(dir).map_or_else(|_| out.to_path_buf(), |dir| dir.join(name))
}

fn sync_file(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("cannot flush {}: {error}", path.display()))
}

fn sync_dir(path: &Path) {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Ok(file) = fs::File::open(dir) {
        let _ = file.sync_all();
    }
}

/// Creates an empty file readable by its owner only. SQLite accepts an existing empty file as a
/// `VACUUM INTO` target, so the copy never exists with a wider mode.
fn create_private(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Copies `db` to `out` with `VACUUM INTO`. The destination is new unless `force` is set, and is
/// never the live database, one of its sidecars or a symlink.
pub fn backup(db: &Path, out: &Path, force: bool) -> Result<BackupInfo, String> {
    if !db.exists() {
        return Err(format!("database {} does not exist", db.display()));
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!(
                "backup directory does not exist: {}",
                parent.display()
            ));
        }
    }
    if live_paths(db).contains(&resolved(out)) {
        return Err("the backup output must not be the database or one of its files".to_owned());
    }
    match fs::symlink_metadata(out) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!("{} is a symlink; refusing to write", out.display()));
        }
        Ok(_) if !force => {
            return Err(format!("{} already exists (use --force)", out.display()));
        }
        Ok(_) => fs::remove_file(out)
            .map_err(|error| format!("cannot replace {}: {error}", out.display()))?,
        Err(_) => {}
    }
    let out_text = out
        .to_str()
        .ok_or_else(|| format!("backup path is not valid UTF-8: {}", out.display()))?;
    create_private(out).map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    let result = (|| {
        let source = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
        source
            .busy_timeout(std::time::Duration::from_secs(10))
            .map_err(|error| error.to_string())?;
        source
            .execute("VACUUM INTO ?1", params![out_text])
            .map_err(|error| format!("cannot back up {}: {error}", db.display()))?;
        drop(source);
        sync_file(out)?;
        verify_integrity(out, "integrity_check")?;
        read_info(out)
    })();
    if result.is_err() {
        // A partial file must not be mistaken for a backup.
        let _ = fs::remove_file(out);
    }
    result
}

/// A backup must be a Lu database: a known schema version and a jobs table. An empty or foreign
/// SQLite file passes `integrity_check`, and installing one would replace the data with nothing.
fn require_lu_database(info: &BackupInfo) -> Result<(), String> {
    match info.schema_version {
        Some(version) if (1..=SCHEMA_VERSION).contains(&version) => Ok(()),
        Some(version) if version > SCHEMA_VERSION => Err(format!(
            "backup schema version {version} is newer than supported {SCHEMA_VERSION}"
        )),
        Some(version) => Err(format!("backup schema version {version} is not valid")),
        None => Err("the file is not a Lu database (no schema version)".to_owned()),
    }
}

/// Installs `backup` at `db` after checking it and moving the current database aside. The new
/// file is staged beside the database and renamed into place, so a crash leaves either the old
/// database or the new one, plus the `pre-restore` copy, never a partial file.
pub fn restore(backup: &Path, db: &Path) -> Result<RestoreInfo, String> {
    if !backup.exists() {
        return Err(format!("backup {} does not exist", backup.display()));
    }
    verify_integrity(backup, "integrity_check")?;
    let info = read_info(backup)?;
    require_lu_database(&info)?;
    if let Some(parent) = db.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!(
                "database directory does not exist: {}",
                parent.display()
            ));
        }
    }

    // One restore at a time, for the whole operation.
    let lock_path = sidecar(db, ".restore.lock");
    let restore_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| format!("cannot open {}: {error}", lock_path.display()))?;
    restore_lock
        .try_lock()
        .map_err(|_| "another restore is running".to_owned())?;

    let owner = fs::metadata(db).ok();
    if db.exists() {
        // The advisory lock catches a second restore; the SQLite exclusive transaction catches a
        // live controller or another writer. Opening the database also folds a crashed
        // controller's WAL into the main file before it is moved aside.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(db)
            .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
        lock.try_lock()
            .map_err(|_| format!("database {} is in use; stop the controller", db.display()))?;
        let conn = Connection::open(db)
            .map_err(|error| format!("cannot open {}: {error}", db.display()))?;
        conn.busy_timeout(std::time::Duration::ZERO)
            .map_err(|error| error.to_string())?;
        conn.execute_batch("BEGIN EXCLUSIVE; ROLLBACK;")
            .map_err(|_| format!("database {} is in use; stop the controller", db.display()))?;
        let busy: i64 = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .map_err(|error| format!("cannot checkpoint {}: {error}", db.display()))?;
        if busy != 0 {
            return Err(format!(
                "database {} is in use; stop the controller",
                db.display()
            ));
        }
        drop(conn);
        drop(lock);
    }

    // Stage the new file beside the database, owned like the old one, and flush it.
    let staged = sidecar(db, ".restore-tmp");
    let _ = fs::remove_file(&staged);
    let stage = || -> Result<(), String> {
        let mut out = create_private(&staged)
            .map_err(|error| format!("cannot create {}: {error}", staged.display()))?;
        let mut input = fs::File::open(backup)
            .map_err(|error| format!("cannot read {}: {error}", backup.display()))?;
        std::io::copy(&mut input, &mut out)
            .map_err(|error| format!("cannot copy {}: {error}", backup.display()))?;
        out.sync_all()
            .map_err(|error| format!("cannot flush {}: {error}", staged.display()))?;
        if let Some(meta) = &owner {
            std::os::unix::fs::chown(&staged, Some(meta.uid()), Some(meta.gid())).map_err(
                |error| format!("cannot give {} the old owner: {error}", staged.display()),
            )?;
        }
        Ok(())
    };
    if let Err(error) = stage() {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }

    // Move the old database and any WAL or SHM aside. A leftover sidecar must never meet the
    // new file, so every one that exists moves, and a failure puts the earlier ones back.
    let moved_base = pre_restore_path(db);
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let from = sidecar(db, suffix);
        if !from.exists() {
            continue;
        }
        let to = sidecar(&moved_base, suffix);
        if let Err(error) = fs::rename(&from, &to) {
            for (from, to) in moved.iter().rev() {
                let _ = fs::rename(to, from);
            }
            let _ = fs::remove_file(&staged);
            return Err(format!("cannot move {} aside: {error}", from.display()));
        }
        moved.push((from, to));
    }
    if let Err(error) = fs::rename(&staged, db) {
        for (from, to) in moved.iter().rev() {
            let _ = fs::rename(to, from);
        }
        let _ = fs::remove_file(&staged);
        return Err(format!(
            "cannot install {}: {error}; the previous database is back in place",
            db.display()
        ));
    }
    sync_dir(db);
    let _ = fs::remove_file(&lock_path);
    let pre_restore = moved
        .iter()
        .any(|(from, _)| from == db)
        .then_some(moved_base);
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
    use std::os::unix::fs::PermissionsExt;
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
            base_commit: None,
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

    #[test]
    fn restore_refuses_an_empty_or_foreign_sqlite_file() {
        let dir = temp_dir("foreign");
        let target = dir.join("lu.db");
        {
            let _store = crate::sqlite::SqliteStore::open(&target).unwrap();
        }
        let empty = dir.join("empty.db");
        fs::write(&empty, b"").unwrap();
        let error = restore(&empty, &target).unwrap_err();
        assert!(error.contains("not a Lu database"), "{error}");
        let foreign = dir.join("foreign.db");
        let conn = Connection::open(&foreign).unwrap();
        conn.execute("CREATE TABLE other(x)", []).unwrap();
        drop(conn);
        let error = restore(&foreign, &target).unwrap_err();
        assert!(error.contains("not a Lu database"), "{error}");
        assert!(target.exists(), "the live database stays in place");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_moves_every_sidecar_aside_and_leaves_no_stage_file() {
        let dir = temp_dir("sidecars");
        let source = dir.join("source.db");
        let copy = dir.join("copy.db");
        {
            let _store = crate::sqlite::SqliteStore::open(&source).unwrap();
        }
        backup(&source, &copy, false).unwrap();
        // The live path has no database, only the leftovers of an old one.
        let target = dir.join("lu.db");
        fs::write(sidecar(&target, "-wal"), b"stale wal frames").unwrap();
        fs::write(sidecar(&target, "-shm"), b"stale shm").unwrap();
        let restored = restore(&copy, &target).unwrap();
        assert_eq!(restored.pre_restore, None, "no database was moved");
        assert!(!sidecar(&target, "-wal").exists());
        assert!(!sidecar(&target, "-shm").exists());
        assert!(!sidecar(&target, ".restore-tmp").exists());
        let mode = fs::metadata(&target).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let reopened = crate::sqlite::SqliteStore::open(&target).unwrap();
        drop(reopened);
        let kept = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.contains("pre-restore") && name.ends_with("-wal")
            });
        assert!(kept, "the old WAL is kept under a pre-restore name");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_refuses_the_live_files_even_with_force_and_dotdot() {
        let dir = temp_dir("liveout");
        let db = dir.join("lu.db");
        {
            let _store = crate::sqlite::SqliteStore::open(&db).unwrap();
        }
        fs::write(sidecar(&db, "-wal"), b"x").unwrap();
        let sneaky = dir.join("sub").join("..").join("lu.db");
        fs::create_dir_all(dir.join("sub")).unwrap();
        for out in [db.clone(), sidecar(&db, "-wal"), sneaky] {
            let error = backup(&db, &out, true).unwrap_err();
            assert!(error.contains("must not be"), "{}: {error}", out.display());
        }
        assert!(sidecar(&db, "-wal").exists(), "the live WAL is untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_refuses_a_symlink_and_has_a_private_mode_from_creation() {
        let dir = temp_dir("symlink");
        let db = dir.join("lu.db");
        {
            let _store = crate::sqlite::SqliteStore::open(&db).unwrap();
        }
        let link = dir.join("link.db");
        std::os::unix::fs::symlink(dir.join("elsewhere.db"), &link).unwrap();
        let error = backup(&db, &link, true).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!dir.join("elsewhere.db").exists());
        let out = dir.join("ok.db");
        backup(&db, &out, false).unwrap();
        assert_eq!(fs::metadata(&out).unwrap().mode() & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_restore_is_refused_while_one_holds_the_lock() {
        let dir = temp_dir("twice");
        let source = dir.join("source.db");
        let copy = dir.join("copy.db");
        {
            let _store = crate::sqlite::SqliteStore::open(&source).unwrap();
        }
        backup(&source, &copy, false).unwrap();
        let target = dir.join("lu.db");
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sidecar(&target, ".restore.lock"))
            .unwrap();
        held.try_lock().unwrap();
        let error = restore(&copy, &target).unwrap_err();
        assert!(error.contains("another restore"), "{error}");
        drop(held);
        restore(&copy, &target).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
