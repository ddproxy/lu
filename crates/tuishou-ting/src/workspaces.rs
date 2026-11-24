//! CI-style lifecycle for the job workspaces under the agent's work directory.
//!
//! Only direct children named by a [`JobId`] are ever considered. Deletion walks with
//! `symlink_metadata`, so a symlink is removed as a link and never followed out of the root.
//! A failed deletion is counted and retried with backoff; it never delays a job start.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lu_proto::{
    printable, truncate_chars, JobId, Outcome, WorkspacePolicy, WorkspaceState, WorkspaceView,
};
use tracing::warn;

use crate::housekeeping::DiskSpace;

/// Where time comes from. Tests use a stepped clock so TTLs do not need sleeping.
pub trait Clock: Send + Sync {
    /// Whole seconds since the Unix epoch.
    fn now_secs(&self) -> u64;
}

/// The production clock.
#[derive(Debug, Default)]
pub struct RealClock;

impl Clock for RealClock {
    fn now_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }
}

/// How a workspace tree is removed. Tests inject a failing implementation.
pub trait Deleter: Send + Sync {
    /// Removes `path`, which must be inside `root`, and returns the bytes reclaimed.
    fn remove_tree(&self, root: &Path, path: &Path) -> io::Result<u64>;
}

/// Removes with `symlink_metadata`, so symlinks are unlinked rather than traversed.
#[derive(Debug, Default)]
pub struct FsDeleter;

impl Deleter for FsDeleter {
    fn remove_tree(&self, root: &Path, path: &Path) -> io::Result<u64> {
        remove_tree(root, path)
    }
}

/// How the registry reads disk use for its high- and low-water checks.
pub type DiskProbe = Arc<dyn Fn(&Path) -> Option<DiskSpace> + Send + Sync>;

/// Agent-side workspace settings, derived from `[workspace]` and `[agent] work_dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub root: PathBuf,
    /// Policy for jobs that omit their `workspace` field.
    pub default_policy: WorkspacePolicy,
    pub failed_ttl: Duration,
    pub keep_ttl: Duration,
    pub max_bytes: Option<u64>,
    pub low_water_percent: u8,
    pub high_water_percent: u8,
    pub sweep_interval: Duration,
}

impl Settings {
    /// Defaults for tests and callers without a `[workspace]` table.
    #[cfg(test)]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            default_policy: WorkspacePolicy::Ephemeral,
            failed_ttl: Duration::from_secs(6 * 3600),
            keep_ttl: Duration::from_secs(48 * 3600),
            max_bytes: None,
            low_water_percent: 65,
            high_water_percent: 80,
            sweep_interval: Duration::from_secs(60),
        }
    }
}

impl Settings {
    /// How often the agent sweeps TTLs, retries and disk pressure.
    pub fn sweep_interval(&self) -> Duration {
        self.sweep_interval
    }
}

#[derive(Debug, Clone)]
struct Entry {
    job_id: JobId,
    epoch: u64,
    path: PathBuf,
    policy: WorkspacePolicy,
    state: WorkspaceState,
    created_at: u64,
    finished_at: Option<u64>,
    /// TTL deadline for kept trees, or a fallback deadline while waiting for an ack.
    deadline: Option<u64>,
    /// Whether automatic deletion waits for the controller's artifact/dependant confirmation.
    needs_ack: bool,
    ack_retry_at: Option<u64>,
    ack_retries: u32,
    retry_at: Option<u64>,
    retries: u32,
    /// Set when this tree was chosen by eviction, so it is counted only once it is gone.
    evicted: bool,
    bytes: u64,
}

impl Entry {
    fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.created_at)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    pub count: u64,
    pub reclaimed_bytes_total: u64,
    pub evictions_total: u64,
    pub delete_errors_total: u64,
}

/// What an operator `clean` request actually did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanResult {
    pub queued: u64,
    /// Jobs whose workspace was left alone because the harness is still running.
    pub refused_running: u64,
    /// Jobs named by the operator that had no workspace.
    pub missing: u64,
}

#[derive(Debug, Default)]
struct Inner {
    entries: BTreeMap<JobId, Entry>,
    stats: Stats,
}

/// A workspace registry owned by one agent. All methods are safe to call from any task.
pub struct Registry {
    settings: Settings,
    root: PathBuf,
    clock: Arc<dyn Clock>,
    deleter: Arc<dyn Deleter>,
    disk: DiskProbe,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("root", &self.root)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Creates the root if needed and canonicalises it, so every later path check compares
    /// against the same absolute prefix.
    pub fn new(
        settings: Settings,
        clock: Arc<dyn Clock>,
        deleter: Arc<dyn Deleter>,
        disk: DiskProbe,
    ) -> Result<Self, String> {
        fs::create_dir_all(&settings.root)
            .map_err(|error| format!("workspace root {}: {error}", settings.root.display()))?;
        let root = fs::canonicalize(&settings.root)
            .map_err(|error| format!("workspace root {}: {error}", settings.root.display()))?;
        Ok(Self {
            settings,
            root,
            clock,
            deleter,
            disk,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// The settings this registry was built with.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The path reserved for one job. Job ids are validated by their type, so this cannot
    /// contain `..` or a separator.
    pub fn path_for(&self, job_id: &JobId) -> PathBuf {
        self.root.join(job_id.as_str())
    }

    fn now(&self) -> u64 {
        self.clock.now_secs()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Creates the job's cache directories. Every path is relative and confined to the tree.
    pub fn create_cache_dirs(&self, job_id: &JobId, dirs: &[String]) -> Result<(), String> {
        let base = self.path_for(job_id);
        for dir in dirs {
            let mut path = base.clone();
            for part in dir.split('/') {
                if part.is_empty() || part == "." || part == ".." {
                    return Err(format!("unsafe cache directory {dir:?}"));
                }
                path.push(part);
                if !path.starts_with(&base) {
                    return Err(format!("cache directory {dir:?} escapes the workspace"));
                }
                match fs::symlink_metadata(&path) {
                    Ok(meta) => {
                        if meta.file_type().is_symlink() {
                            return Err(format!("cache directory {dir:?} is a symlink"));
                        }
                        if !meta.is_dir() {
                            return Err(format!("cache directory {dir:?} is not a directory"));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::DirBuilderExt;
                            std::fs::DirBuilder::new()
                                .mode(0o700)
                                .create(&path)
                                .map_err(|error| {
                                    format!("cannot create cache directory {dir:?}: {error}")
                                })?;
                        }
                        #[cfg(not(unix))]
                        {
                            fs::create_dir(&path).map_err(|error| {
                                format!("cannot create cache directory {dir:?}: {error}")
                            })?;
                        }
                        let meta = fs::symlink_metadata(&path).map_err(|error| {
                            format!("cannot stat cache directory {dir:?}: {error}")
                        })?;
                        if meta.file_type().is_symlink() || !meta.is_dir() {
                            return Err(format!("cache directory {dir:?} is unsafe"));
                        }
                    }
                    Err(error) => {
                        return Err(format!("cannot stat cache directory {dir:?}: {error}"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Starts tracking a staged tree. Call before the harness runs so eviction never races a
    /// running job.
    pub fn register(&self, job_id: &JobId, epoch: u64, policy: WorkspacePolicy) {
        let now = self.now();
        let path = self.path_for(job_id);
        let bytes = tree_bytes(&path);
        let mut inner = self.lock();
        inner.entries.insert(
            job_id.clone(),
            Entry {
                job_id: job_id.clone(),
                epoch,
                path,
                policy,
                state: WorkspaceState::Running,
                created_at: now,
                finished_at: None,
                deadline: None,
                needs_ack: false,
                ack_retry_at: None,
                ack_retries: 0,
                retry_at: None,
                retries: 0,
                evicted: false,
                bytes,
            },
        );
        self.refresh_stats(&mut inner);
    }

    /// Resolves a job's optional policy against the configured default.
    pub fn resolve_policy(&self, job: Option<WorkspacePolicy>) -> WorkspacePolicy {
        job.unwrap_or(self.settings.default_policy)
    }

    /// Marks a job's workspace finished and decides whether it is kept, waiting for an ack, or
    /// already eligible for deletion.
    pub fn finish(&self, job_id: &JobId, outcome: Outcome) {
        let now = self.now();
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get_mut(job_id) else {
            return;
        };
        entry.finished_at = Some(now);
        entry.bytes = tree_bytes(&entry.path);
        let succeeded = matches!(outcome, Outcome::Done);
        match entry.policy {
            WorkspacePolicy::Keep => {
                entry.state = WorkspaceState::Kept;
                entry.deadline = Some(now.saturating_add(self.settings.keep_ttl.as_secs()));
                entry.needs_ack = false;
            }
            WorkspacePolicy::KeepOnFailure if !succeeded => {
                entry.state = WorkspaceState::Kept;
                entry.deadline = Some(now.saturating_add(self.settings.failed_ttl.as_secs()));
                entry.needs_ack = false;
            }
            WorkspacePolicy::KeepOnFailure => {
                entry.state = WorkspaceState::Waiting;
                entry.needs_ack = true;
                entry.ack_retry_at = Some(now);
                entry.deadline = Some(now.saturating_add(self.settings.failed_ttl.as_secs()));
            }
            WorkspacePolicy::Ephemeral => {
                entry.state = WorkspaceState::Waiting;
                entry.needs_ack = true;
                entry.ack_retry_at = Some(now);
                entry.deadline = None;
            }
        }
        self.refresh_stats(&mut inner);
    }

    /// Entries whose controller confirmation is due, oldest first.
    pub fn pending_acks(&self) -> Vec<(JobId, u64)> {
        let now = self.now();
        let mut inner = self.lock();
        let mut due: Vec<Entry> = inner
            .entries
            .values()
            .filter(|entry| entry.needs_ack && entry.ack_retry_at.is_some_and(|at| at <= now))
            .cloned()
            .collect();
        due.sort_by_key(|entry| (entry.created_at, entry.job_id.clone()));
        for entry in &due {
            if let Some(live) = inner.entries.get_mut(&entry.job_id) {
                // One attempt per sweep, even when the controller call is slow.
                live.ack_retry_at = Some(now + backoff(live.ack_retries));
                live.ack_retries = live.ack_retries.saturating_add(1);
            }
        }
        due.into_iter()
            .map(|entry| (entry.job_id, entry.epoch))
            .collect()
    }

    /// Applies the controller's answer. `can_delete = false` keeps the tree waiting; the reason
    /// is only logged as sanitised text and never stored.
    pub fn acknowledge(&self, job_id: &JobId, can_delete: bool, reason: &str) {
        let now = self.now();
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get_mut(job_id) else {
            return;
        };
        if !entry.needs_ack {
            return;
        }
        if can_delete {
            entry.needs_ack = false;
            entry.state = WorkspaceState::Deleting;
            entry.retry_at = Some(now);
        } else {
            warn!(
                job = %job_id,
                reason = %truncate_chars(&printable(reason), 160),
                "workspace kept; controller has not released it yet"
            );
        }
    }

    /// Queues a finished workspace for deletion because its TTL expired.
    fn expire(&self, entry: &mut Entry, now: u64) {
        if matches!(
            entry.state,
            WorkspaceState::Running | WorkspaceState::Deleting
        ) {
            return;
        }
        entry.needs_ack = false;
        entry.state = WorkspaceState::Deleting;
        entry.retry_at = Some(now);
    }

    /// Sweeps TTLs, retries, disk pressure and the optional byte cap. Returns the number of
    /// workspaces actually removed in this pass.
    pub fn sweep(&self) -> u64 {
        let now = self.now();
        let due: Vec<JobId> = {
            let inner = self.lock();
            inner
                .entries
                .values()
                .filter(|entry| match entry.state {
                    WorkspaceState::Deleting => entry.retry_at.is_some_and(|at| at <= now),
                    WorkspaceState::Kept | WorkspaceState::Waiting => {
                        entry.deadline.is_some_and(|at| at <= now)
                    }
                    WorkspaceState::Running => false,
                })
                .map(|entry| entry.job_id.clone())
                .collect()
        };
        for job_id in due {
            let mut inner = self.lock();
            let expire = inner
                .entries
                .get(&job_id)
                .is_some_and(|entry| !matches!(entry.state, WorkspaceState::Deleting));
            if expire {
                if let Some(entry) = inner.entries.get_mut(&job_id) {
                    self.expire(entry, now);
                }
            }
            drop(inner);
            self.delete_one(&job_id);
        }

        self.apply_pressure(now);
        self.lock().stats.count
    }

    fn apply_pressure(&self, now: u64) {
        let used_percent = || {
            (self.disk)(&self.root).map(|space| {
                let free_percent = space.free_percent().min(100);
                100u8.saturating_sub(u8::try_from(free_percent).unwrap_or(100))
            })
        };
        let pressure =
            used_percent().is_some_and(|value| value >= self.settings.high_water_percent);
        let mut inner = self.lock();
        let total: u64 = inner.entries.values().map(|entry| entry.bytes).sum();
        let over_cap = self.settings.max_bytes.is_some_and(|cap| total > cap);
        if !pressure && !over_cap {
            return;
        }
        let low = self.settings.low_water_percent;
        let cap = self.settings.max_bytes.unwrap_or(u64::MAX);
        let mut candidates: Vec<Entry> = inner
            .entries
            .values()
            .filter(|entry| {
                !matches!(
                    entry.state,
                    WorkspaceState::Running | WorkspaceState::Deleting
                )
            })
            .cloned()
            .collect();
        candidates.sort_by_key(|entry| (entry.created_at, entry.job_id.clone()));
        for entry in candidates {
            let still_pressured = used_percent().is_some_and(|value| value > low)
                || inner.entries.values().map(|e| e.bytes).sum::<u64>() > cap;
            if !still_pressured {
                break;
            }
            if let Some(live) = inner.entries.get_mut(&entry.job_id) {
                live.evicted = true;
                self.expire(live, now);
            }
            drop(inner);
            self.delete_one(&entry.job_id);
            inner = self.lock();
        }
    }

    fn delete_one(&self, job_id: &JobId) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get_mut(job_id) else {
            return false;
        };
        if matches!(entry.state, WorkspaceState::Running) {
            return false;
        }
        entry.state = WorkspaceState::Deleting;
        drop(inner);

        let (path, bytes, evicted) = {
            let inner = self.lock();
            let Some(entry) = inner.entries.get(job_id) else {
                return false;
            };
            (entry.path.clone(), entry.bytes, entry.evicted)
        };
        match self.deleter.remove_tree(&self.root, &path) {
            Ok(reclaimed) => {
                let mut inner = self.lock();
                inner.stats.reclaimed_bytes_total = inner
                    .stats
                    .reclaimed_bytes_total
                    .saturating_add(reclaimed.max(bytes));
                if evicted {
                    inner.stats.evictions_total = inner.stats.evictions_total.saturating_add(1);
                }
                inner.entries.remove(job_id);
                self.refresh_stats(&mut inner);
                true
            }
            Err(error) => {
                warn!(
                    job = %job_id,
                    error = %truncate_chars(&printable(&error.to_string()), 200),
                    "workspace deletion failed; will retry"
                );
                let mut inner = self.lock();
                if let Some(entry) = inner.entries.get_mut(job_id) {
                    entry.retries = entry.retries.saturating_add(1);
                    entry.retry_at = Some(self.now() + backoff(entry.retries));
                }
                inner.stats.delete_errors_total = inner.stats.delete_errors_total.saturating_add(1);
                false
            }
        }
    }

    /// Operator clean request. Running jobs are refused; everything else is queued for the
    /// sweeper so a slow filesystem never blocks the poll loop.
    pub fn clean(&self, all: bool, job: Option<&JobId>) -> CleanResult {
        let now = self.now();
        let mut inner = self.lock();
        let mut result = CleanResult::default();
        let targets: Vec<JobId> = match job {
            Some(id) => vec![id.clone()],
            None if all => inner.entries.keys().cloned().collect(),
            None => return result,
        };
        for id in targets {
            match inner.entries.get_mut(&id) {
                None => result.missing += 1,
                Some(entry) if matches!(entry.state, WorkspaceState::Running) => {
                    result.refused_running += 1;
                }
                Some(entry) => {
                    entry.needs_ack = false;
                    entry.state = WorkspaceState::Deleting;
                    entry.retry_at = Some(now);
                    result.queued += 1;
                }
            }
        }
        result
    }

    /// Paths the housekeeper must never touch: every tree the registry still owns, including
    /// running workspaces and those queued for deletion.
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        self.lock()
            .entries
            .values()
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Counters for the next heartbeat.
    pub fn stats(&self) -> Stats {
        self.lock().stats
    }

    /// The listing sent with heartbeats and served by `lu node <name> --workspaces`.
    pub fn list(&self) -> Vec<WorkspaceView> {
        let now = self.now();
        let inner = self.lock();
        let mut entries: Vec<&Entry> = inner.entries.values().collect();
        entries.sort_by_key(|entry| (entry.created_at, entry.job_id.clone()));
        entries
            .into_iter()
            .map(|entry| WorkspaceView {
                job_key: entry.job_id.to_string(),
                state: entry.state,
                bytes: entry.bytes,
                age_secs: entry.age_secs(now),
                policy: entry.policy,
            })
            .collect()
    }

    fn refresh_stats(&self, inner: &mut Inner) {
        let bytes = inner.entries.values().map(|entry| entry.bytes).sum();
        inner.stats.bytes = bytes;
        inner.stats.count = u64::try_from(inner.entries.len()).unwrap_or(u64::MAX);
    }
}

fn backoff(retries: u32) -> u64 {
    if retries == 0 {
        return 1;
    }
    1u64.checked_shl(retries.min(6)).unwrap_or(64).min(64)
}

/// Recursively sums file sizes without following symlinks. A symlink counts as the link itself.
pub fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return meta.len();
    }
    let Ok(children) = fs::read_dir(path) else {
        return meta.len();
    };
    let mut total = meta.len();
    for child in children.flatten() {
        total = total.saturating_add(tree_bytes(&child.path()));
    }
    total
}

/// Removes one tree, refusing anything outside `root` and never removing `root` itself.
pub fn remove_tree(root: &Path, path: &Path) -> io::Result<u64> {
    if path == root {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to delete root",
        ));
    }
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path is outside the workspace root",
        )
    })?;
    if relative.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to delete root",
        ));
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        let bytes = meta.len();
        fs::remove_file(path)?;
        return Ok(bytes);
    }
    let mut reclaimed = meta.len();
    for child in fs::read_dir(path)? {
        let child = child?;
        reclaimed = reclaimed.saturating_add(remove_tree(root, &child.path())?);
    }
    fs::remove_dir(path)?;
    Ok(reclaimed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn id(n: u8) -> JobId {
        let hex = format!("01ARZ3NDEKTSV4RRFFQ69G5F{n:02X}");
        JobId::parse(&hex).unwrap()
    }

    struct StepClock(AtomicU64);

    impl Clock for StepClock {
        fn now_secs(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl StepClock {
        fn set(&self, value: u64) {
            self.0.store(value, Ordering::SeqCst);
        }
    }

    struct FailingDeleter;

    impl Deleter for FailingDeleter {
        fn remove_tree(&self, _root: &Path, _path: &Path) -> io::Result<u64> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated failure",
            ))
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        clock: Arc<StepClock>,
        settings: Settings,
        disk: AtomicU64,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let clock = Arc::new(StepClock(AtomicU64::new(1000)));
            let mut settings = Settings::new(dir.path().join("work"));
            settings.failed_ttl = Duration::from_secs(6);
            settings.keep_ttl = Duration::from_secs(10);
            let disk = AtomicU64::new(10);
            Self {
                dir,
                clock,
                settings,
                disk,
            }
        }

        fn registry(&self) -> Registry {
            let disk_used = Arc::new(AtomicU64::new(self.disk.load(Ordering::SeqCst)));
            let disk = disk_used.clone();
            Registry::new(
                self.settings.clone(),
                self.clock.clone(),
                Arc::new(FsDeleter),
                Arc::new(move |_| {
                    let used = disk.load(Ordering::SeqCst);
                    Some(DiskSpace {
                        free_bytes: (100 - used).saturating_mul(1024),
                        total_bytes: 100 * 1024,
                    })
                }),
            )
            .unwrap()
        }

        fn stub(&self, job: &JobId, bytes: usize) -> PathBuf {
            let path = self.settings.root.join(job.as_str());
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("blob.txt"), vec![b'x'; bytes]).unwrap();
            path
        }
    }

    #[test]
    fn ephemeral_waits_for_acknowledgement_then_deletes() {
        let f = Fixture::new();
        let r = f.registry();
        let job = id(1);
        let path = f.stub(&job, 64);
        r.register(&job, 1, WorkspacePolicy::Ephemeral);
        r.finish(&job, Outcome::Done);
        assert_eq!(
            r.list()[0].state,
            WorkspaceState::Waiting,
            "final artifacts must be acknowledged first"
        );
        assert!(path.exists());
        r.acknowledge(&job, true, "stored");
        r.sweep();
        assert!(!path.exists());
        assert!(r.stats().reclaimed_bytes_total > 0);
        assert_eq!(r.stats().count, 0);
    }

    #[test]
    fn dependant_holds_an_ephemeral_workspace_until_released() {
        let f = Fixture::new();
        let r = f.registry();
        let job = id(2);
        let path = f.stub(&job, 8);
        r.register(&job, 1, WorkspacePolicy::Ephemeral);
        r.finish(&job, Outcome::Done);
        r.acknowledge(&job, false, "dependants still running");
        r.sweep();
        assert!(path.exists());
        r.acknowledge(&job, true, "dependants done");
        r.sweep();
        assert!(!path.exists());
    }

    #[test]
    fn keep_on_failure_keeps_failures_and_deletes_successes() {
        let f = Fixture::new();
        let r = f.registry();
        let failed = id(3);
        let ok = id(4);
        let failed_path = f.stub(&failed, 16);
        let ok_path = f.stub(&ok, 16);
        r.register(&failed, 1, WorkspacePolicy::KeepOnFailure);
        r.finish(&failed, Outcome::Failed(lu_proto::FailureClass::Execution));
        r.register(&ok, 2, WorkspacePolicy::KeepOnFailure);
        r.finish(&ok, Outcome::Done);
        assert_eq!(r.list().len(), 2);
        let ok_state = r
            .list()
            .into_iter()
            .find(|w| w.job_key == ok.to_string())
            .unwrap()
            .state;
        assert_eq!(ok_state, WorkspaceState::Waiting);
        r.acknowledge(&ok, true, "stored");
        r.sweep();
        assert!(!ok_path.exists());
        assert!(failed_path.exists());
        f.clock.set(1000 + 5);
        r.sweep();
        assert!(failed_path.exists());
        f.clock.set(1000 + 7);
        r.sweep();
        assert!(!failed_path.exists());
    }

    #[test]
    fn keep_uses_keep_ttl_and_injected_clock() {
        let f = Fixture::new();
        let r = f.registry();
        let job = id(5);
        let path = f.stub(&job, 32);
        r.register(&job, 1, WorkspacePolicy::Keep);
        r.finish(&job, Outcome::Done);
        f.clock.set(1009);
        r.sweep();
        assert!(path.exists());
        f.clock.set(1011);
        r.sweep();
        assert!(!path.exists());
    }

    #[test]
    fn configured_default_policy_applies_to_omitted_job_policy() {
        let f = Fixture::new();
        let mut settings = f.settings.clone();
        settings.default_policy = WorkspacePolicy::Keep;
        let r = Registry::new(
            settings,
            f.clock.clone(),
            Arc::new(FsDeleter),
            Arc::new(|_| None),
        )
        .unwrap();
        assert_eq!(r.resolve_policy(None), WorkspacePolicy::Keep);
        assert_eq!(
            r.resolve_policy(Some(WorkspacePolicy::Ephemeral)),
            WorkspacePolicy::Ephemeral
        );
    }

    #[test]
    fn eviction_is_oldest_first_and_stops_at_low_water() {
        let f = Fixture::new();
        let r = f.registry();
        let mut paths = Vec::new();
        for (n, age) in [(1u8, 0u64), (2, 10), (3, 20), (4, 30)] {
            let job = id(n);
            f.clock.set(1000 + age);
            let path = f.stub(&job, 4);
            r.register(&job, 1, WorkspacePolicy::Keep);
            r.finish(&job, Outcome::Done);
            paths.push(path);
        }
        // Start evicting above 80%, stop at 65%: remove two trees, then stop.
        let r_high = Registry::new(
            f.settings.clone(),
            f.clock.clone(),
            Arc::new(FsDeleter),
            Arc::new(|root| {
                let remaining = fs::read_dir(root).map_or(4, Iterator::count);
                let used = 85u64
                    .saturating_sub(10u64.saturating_mul(4u64.saturating_sub(remaining as u64)));
                Some(DiskSpace {
                    free_bytes: (100 - used.min(100)) * 1024,
                    total_bytes: 100 * 1024,
                })
            }),
        )
        .unwrap();
        for (n, _age) in [(1u8, 0u64), (2, 10), (3, 20), (4, 30)] {
            let job = id(n);
            r_high.register(&job, 1, WorkspacePolicy::Keep);
            r_high.finish(&job, Outcome::Done);
        }
        r_high.sweep();
        assert!(!paths[0].exists(), "oldest goes first");
        assert!(!paths[1].exists());
        assert!(paths[2].exists(), "stop once below low water");
        assert!(paths[3].exists());
        assert_eq!(r_high.stats().evictions_total, 2);
        assert!(r.stats().evictions_total == 0);
    }

    #[test]
    fn running_workspaces_are_never_deleted() {
        let f = Fixture::new();
        let r = f.registry();
        let job = id(6);
        let path = f.stub(&job, 4);
        r.register(&job, 1, WorkspacePolicy::Ephemeral);
        f.disk.store(95, Ordering::SeqCst);
        r.clean(true, None);
        r.sweep();
        assert!(path.exists());
        assert_eq!(r.clean(false, Some(&job)).refused_running, 1);
    }

    #[test]
    fn protected_paths_cover_running_and_draining_workspaces() {
        let f = Fixture::new();
        let r = f.registry();
        let running = id(12);
        let draining = id(13);
        let running_path = f.stub(&running, 4);
        let draining_path = f.stub(&draining, 4);
        r.register(&running, 1, WorkspacePolicy::Keep);
        r.register(&draining, 2, WorkspacePolicy::Ephemeral);
        r.finish(&draining, Outcome::Done);
        r.acknowledge(&draining, true, "stored");
        let protected = r.protected_paths();
        assert!(protected.contains(&running_path));
        assert!(protected.contains(&draining_path));
        assert_eq!(
            r.list()[1].state,
            WorkspaceState::Deleting,
            "acknowledged workspaces are draining until the sweeper removes them"
        );
    }

    #[test]
    fn symlinks_are_unlinked_not_followed() {
        let f = Fixture::new();
        let outside = f.dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let job = id(7);
        let path = f.stub(&job, 1);
        std::os::unix::fs::symlink(&outside, path.join("escape")).unwrap();
        let r = f.registry();
        r.register(&job, 1, WorkspacePolicy::Ephemeral);
        r.finish(&job, Outcome::Done);
        r.acknowledge(&job, true, "stored");
        r.sweep();
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(outside.join("keep.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn cache_dirs_reject_symlink_escape() {
        let f = Fixture::new();
        let r = f.registry();
        let job = id(90);
        let base = r.path_for(&job);
        fs::create_dir_all(&base).unwrap();
        let outside = f.dir.path().join("outside-cache");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("escape")).unwrap();
        let err = r
            .create_cache_dirs(&job, &["escape/subdir".to_owned()])
            .unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(!outside.join("subdir").exists());
    }

    #[test]
    fn traversal_outside_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"safe").unwrap();
        let error = remove_tree(&root, &outside).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(outside.exists());
        assert!(remove_tree(&root, &root).is_err());
    }

    #[test]
    fn deletion_failures_are_counted_and_retried_with_backoff() {
        let f = Fixture::new();
        let settings = f.settings.clone();
        let clock = f.clock.clone();
        let r = Registry::new(
            settings,
            clock,
            Arc::new(FailingDeleter),
            Arc::new(|_| None),
        )
        .unwrap();
        let job = id(8);
        let path = f.stub(&job, 8);
        r.register(&job, 1, WorkspacePolicy::Ephemeral);
        r.finish(&job, Outcome::Done);
        r.acknowledge(&job, true, "stored");
        r.sweep();
        assert_eq!(r.stats().delete_errors_total, 1);
        assert_eq!(r.stats().count, 1);
        assert!(path.exists());
        // First retry is one second later; a sweep before then does not touch the tree again.
        r.sweep();
        assert_eq!(r.stats().delete_errors_total, 1);
        f.clock.set(1002);
        r.sweep();
        assert_eq!(r.stats().delete_errors_total, 2);
    }

    #[test]
    fn operator_clean_queues_kept_workspaces_and_reports_refusals() {
        let f = Fixture::new();
        let r = f.registry();
        let kept = id(9);
        let running = id(10);
        let kept_path = f.stub(&kept, 4);
        let running_path = f.stub(&running, 4);
        r.register(&kept, 1, WorkspacePolicy::Keep);
        r.finish(&kept, Outcome::Done);
        r.register(&running, 2, WorkspacePolicy::Keep);
        let result = r.clean(true, None);
        assert_eq!(result.queued, 1);
        assert_eq!(result.refused_running, 1);
        r.sweep();
        assert!(!kept_path.exists());
        assert!(running_path.exists());
    }

    #[test]
    fn listing_reports_key_state_age_bytes_and_policy() {
        let f = Fixture::new();
        let r = f.registry();
        let job = id(11);
        f.stub(&job, 64);
        r.register(&job, 1, WorkspacePolicy::KeepOnFailure);
        f.clock.set(1030);
        let item = r.list().remove(0);
        assert_eq!(item.job_key, job.to_string());
        assert_eq!(item.state, WorkspaceState::Running);
        assert_eq!(item.policy, WorkspacePolicy::KeepOnFailure);
        assert_eq!(item.age_secs, 30);
        assert!(item.bytes >= 64);
    }
}
