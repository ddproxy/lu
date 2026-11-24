//! Opt-in removal of leftover scratch directories between jobs.
//!
//! A pass only looks at direct children of a configured pattern's parent and only removes
//! regular directories. Everything else, including symlinks, project roots, registered job
//! workspaces and the agent's work directory, is left alone. Any error ends that rule's pass
//! instead of failing the agent.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use lu_config::HousekeepingConfig;
use tracing::{info, warn};

/// Runs before the first poll and after each job, but no more often than this.
pub const MIN_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Extra headroom above `min_free_percent` that a removal pass aims for.
const TARGET_HEADROOM_PERCENT: u64 = 5;

/// Parents too broad to sweep, even when an operator names one explicitly.
const TOO_BROAD: [&str; 5] = ["/", "/home", "/var", "/tmp", "/srv"];

/// Free and total bytes on the filesystem holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl DiskSpace {
    pub fn free_percent(&self) -> u64 {
        self.free_bytes
            .saturating_mul(100)
            .checked_div(self.total_bytes)
            .unwrap_or(0)
    }
}

/// A fixed absolute path to `df`, so a reading never depends on the caller's `PATH`.
#[cfg(unix)]
fn df_binary() -> Option<&'static Path> {
    const CANDIDATES: [&str; 4] = ["/usr/bin/df", "/bin/df", "/usr/sbin/df", "/sbin/df"];
    CANDIDATES
        .iter()
        .map(Path::new)
        .find(|candidate| candidate.is_file())
}

/// Reads the filesystem holding `path`.
#[cfg(unix)]
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    // `std` has no statvfs and this workspace forbids unsafe, so ask the system `df` for the
    // POSIX 1024-byte totals. A failure only costs the optional disk fields.
    let output = std::process::Command::new(df_binary()?)
        .arg("-Pk")
        .arg("--")
        .arg(path)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().skip(1).find(|line| !line.trim().is_empty())?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let total_kb: u64 = fields.get(1)?.parse().ok()?;
    let free_kb: u64 = fields.get(3)?.parse().ok()?;
    Some(DiskSpace {
        free_bytes: free_kb.saturating_mul(1024),
        total_bytes: total_kb.saturating_mul(1024),
    })
}

#[cfg(not(unix))]
pub fn disk_space(_path: &Path) -> Option<DiskSpace> {
    None
}

type FreeSpaceFn = dyn Fn(&Path) -> Option<DiskSpace> + Send + Sync;

/// Metadata a housekeeping decision needs. A symlink is always reported as such and is never
/// followed by the filesystem seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsMeta {
    pub is_dir: bool,
    pub is_symlink: bool,
    pub modified: Option<SystemTime>,
    pub len: u64,
}

/// The filesystem operations housekeeping is allowed to perform. Tests use an in-memory tree
/// so every safety rule can be checked without touching a real disk.
pub trait FsOps: Send + Sync {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn symlink_metadata(&self, path: &Path) -> io::Result<FsMeta>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
}

#[derive(Debug, Default)]
pub struct RealFs;

impl FsOps for RealFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FsMeta> {
        let meta = fs::symlink_metadata(path)?;
        let modified = meta.modified().ok();
        Ok(FsMeta {
            is_dir: meta.is_dir(),
            is_symlink: meta.file_type().is_symlink(),
            modified,
            len: meta.len(),
        })
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}

/// Wall-clock and monotonic time, injected so age and interval tests never sleep.
pub trait TimeSource: Send + Sync {
    fn now(&self) -> SystemTime;
    fn monotonic(&self) -> Instant;
}

#[derive(Debug, Default)]
pub struct RealTime;

impl TimeSource for RealTime {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn monotonic(&self) -> Instant {
        Instant::now()
    }
}

/// Cumulative housekeeping counters, shared with the agent's heartbeat builders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcCounts {
    pub removed_total: u64,
    pub bytes_freed_total: u64,
}

#[derive(Debug, Default)]
pub struct GcCounters {
    removed_total: AtomicU64,
    bytes_freed_total: AtomicU64,
}

impl GcCounters {
    pub fn snapshot(&self) -> GcCounts {
        GcCounts {
            removed_total: self.removed_total.load(Ordering::Relaxed),
            bytes_freed_total: self.bytes_freed_total.load(Ordering::Relaxed),
        }
    }

    fn record(&self, summary: PassSummary) {
        self.removed_total
            .fetch_add(summary.removed as u64, Ordering::Relaxed);
        self.bytes_freed_total
            .fetch_add(summary.freed_bytes, Ordering::Relaxed);
    }
}

/// What one due pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassSummary {
    pub removed: usize,
    pub freed_bytes: u64,
    pub failed: usize,
}

struct Rule {
    parent: PathBuf,
    pattern: String,
    /// Whether the rule names build output, the only case where a project marker is tolerated.
    target_pattern: bool,
}

/// Stateful housekeeping for one agent. `run` is a no-op while a job is active or within
/// [`MIN_INTERVAL`] of the previous pass.
pub struct Housekeeper {
    rules: Vec<Rule>,
    min_age_hours: u64,
    min_free_percent: u64,
    max_remove_per_pass: usize,
    free_space: Arc<FreeSpaceFn>,
    fs: Arc<dyn FsOps>,
    time: Arc<dyn TimeSource>,
    counters: Arc<GcCounters>,
    last_pass: Option<Instant>,
}

impl Housekeeper {
    pub fn new(config: HousekeepingConfig) -> Self {
        Self::with_free_space(config, disk_space)
    }

    pub fn with_free_space<F>(config: HousekeepingConfig, free_space: F) -> Self
    where
        F: Fn(&Path) -> Option<DiskSpace> + Send + Sync + 'static,
    {
        Self::with_io(config, free_space, Arc::new(RealFs), Arc::new(RealTime))
    }

    /// Full injection seam: filesystem, clock and free-space probe are all replaceable.
    pub fn with_io<F>(
        config: HousekeepingConfig,
        free_space: F,
        fs: Arc<dyn FsOps>,
        time: Arc<dyn TimeSource>,
    ) -> Self
    where
        F: Fn(&Path) -> Option<DiskSpace> + Send + Sync + 'static,
    {
        let mut rules: Vec<Rule> = config
            .scratch_globs
            .iter()
            .filter_map(|glob| {
                let path = Path::new(glob);
                let pattern = path.file_name()?.to_string_lossy().into_owned();
                Some(Rule {
                    parent: path.parent()?.to_path_buf(),
                    target_pattern: is_target_pattern(&pattern),
                    pattern,
                })
            })
            .collect();
        // Harness containers write build output into a bind-mounted host path. The mount is
        // configured as an extra parent; only its `target*` children are eligible.
        rules.extend(config.extra_roots.into_iter().map(|root| Rule {
            parent: PathBuf::from(root),
            pattern: "target*".to_owned(),
            target_pattern: true,
        }));
        Self {
            rules,
            min_age_hours: config.min_age_hours,
            min_free_percent: config.min_free_percent,
            max_remove_per_pass: config.max_remove_per_pass,
            free_space: Arc::new(free_space),
            fs,
            time,
            counters: Arc::new(GcCounters::default()),
            last_pass: None,
        }
    }

    /// A shared view of cumulative removals, for both idle and busy heartbeats.
    pub fn counters(&self) -> Arc<GcCounters> {
        Arc::clone(&self.counters)
    }

    /// Runs a pass when due. `None` means it was skipped because a job is active or the last
    /// pass was too recent. `protected` holds workspace trees that must never be touched.
    pub fn run(
        &mut self,
        work_dir: &Path,
        job_running: bool,
        protected: &[PathBuf],
    ) -> Option<PassSummary> {
        if job_running {
            return None;
        }
        let now = self.time.monotonic();
        if self
            .last_pass
            .is_some_and(|last| now.duration_since(last) < MIN_INTERVAL)
        {
            return None;
        }
        self.last_pass = Some(now);
        let summary = self.pass(work_dir, protected);
        self.counters.record(summary);
        Some(summary)
    }

    fn pass(&self, work_dir: &Path, protected: &[PathBuf]) -> PassSummary {
        let work_canon = self.fs.canonicalize(work_dir).ok();
        let guarded: Vec<PathBuf> = protected
            .iter()
            .filter_map(|path| self.fs.canonicalize(path).ok())
            .collect();
        let mut summary = PassSummary::default();
        'rules: for rule in &self.rules {
            let Ok(parent) = self.fs.canonicalize(&rule.parent) else {
                continue;
            };
            if is_too_broad(&parent) {
                warn!(path = %parent.display(), "refusing a housekeeping root that is too broad");
                continue;
            }
            if self.min_free_percent > 0 {
                let Some(space) = (self.free_space)(&parent) else {
                    continue;
                };
                if space.free_percent() >= self.min_free_percent {
                    continue;
                }
            }
            let target = self
                .min_free_percent
                .saturating_add(TARGET_HEADROOM_PERCENT)
                .min(100);
            let mut candidates = self.candidates(rule, &parent, work_canon.as_deref(), &guarded);
            candidates.sort_by_key(|candidate| candidate.newest);
            for candidate in candidates {
                if summary.removed >= self.max_remove_per_pass {
                    break 'rules;
                }
                if self.min_free_percent > 0 {
                    let Some(current) = (self.free_space)(&parent) else {
                        break;
                    };
                    if current.free_percent() >= target {
                        break;
                    }
                }
                match self.fs.remove_dir_all(&candidate.canonical) {
                    Ok(()) => {
                        info!(
                            path = %candidate.path.display(),
                            size_bytes = candidate.size_bytes,
                            age_secs = candidate.age.as_secs(),
                            "removed scratch directory"
                        );
                        summary.removed += 1;
                        summary.freed_bytes =
                            summary.freed_bytes.saturating_add(candidate.size_bytes);
                    }
                    Err(error) => {
                        warn!(
                            path = %candidate.path.display(),
                            error = %error,
                            "could not remove scratch directory"
                        );
                        summary.failed += 1;
                    }
                }
            }
        }
        info!(
            scanned = self.rules.len(),
            removed = summary.removed,
            freed_bytes = summary.freed_bytes,
            failed = summary.failed,
            "housekeeping pass complete"
        );
        summary
    }

    fn candidates(
        &self,
        rule: &Rule,
        parent: &Path,
        work_canon: Option<&Path>,
        guarded: &[PathBuf],
    ) -> Vec<Candidate> {
        let now = self.time.now();
        let Ok(entries) = self.fs.read_dir(parent) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for path in entries {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !glob_matches(&rule.pattern, name) {
                continue;
            }
            let Ok(meta) = self.fs.symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_dir || meta.is_symlink {
                continue;
            }
            let Ok(canonical) = self.fs.canonicalize(&path) else {
                continue;
            };
            // A direct child of the canonical parent is the longest path that can be removed
            // without escaping the configured root through a link or a path component.
            if canonical.parent() != Some(parent) || canonical == parent {
                continue;
            }
            if work_canon.is_some_and(|work| overlaps(&canonical, work))
                || guarded.iter().any(|g| overlaps(&canonical, g))
            {
                continue;
            }
            let has_marker = [".git", "Cargo.toml"]
                .iter()
                .any(|marker| self.fs.symlink_metadata(&path.join(marker)).is_ok());
            if has_marker && !(rule.target_pattern && is_target_name(name)) {
                continue;
            }
            let Some((newest, size_bytes)) = self.scan_tree(&path) else {
                continue;
            };
            let age = now.duration_since(newest).unwrap_or(Duration::ZERO);
            if age < Duration::from_secs(self.min_age_hours.saturating_mul(3600)) {
                continue;
            }
            found.push(Candidate {
                path,
                canonical,
                newest,
                size_bytes,
                age,
            });
        }
        found
    }

    /// Newest modification time and apparent size of a directory tree, without following links.
    /// `None` means part of the tree could not be read, so the entry is not safe to remove.
    fn scan_tree(&self, root: &Path) -> Option<(SystemTime, u64)> {
        let mut newest = None;
        let mut size_bytes = 0u64;
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = self.fs.symlink_metadata(&path).ok()?;
            if meta.is_symlink {
                continue;
            }
            if let Some(modified) = meta.modified {
                newest = Some(match newest {
                    Some(current) if current >= modified => current,
                    _ => modified,
                });
            }
            if meta.is_dir {
                for child in self.fs.read_dir(&path).ok()? {
                    stack.push(child);
                }
            } else {
                size_bytes = size_bytes.saturating_add(meta.len);
            }
        }
        Some((newest?, size_bytes))
    }
}

struct Candidate {
    path: PathBuf,
    canonical: PathBuf,
    newest: SystemTime,
    size_bytes: u64,
    age: Duration,
}

fn is_too_broad(path: &Path) -> bool {
    TOO_BROAD.iter().any(|wide| path == Path::new(wide))
}

fn overlaps(entry: &Path, guarded: &Path) -> bool {
    entry == guarded || entry.starts_with(guarded) || guarded.starts_with(entry)
}

fn is_target_pattern(pattern: &str) -> bool {
    pattern == "target" || pattern == "target*" || pattern.starts_with("target-")
}

fn is_target_name(name: &str) -> bool {
    name == "target" || name.starts_with("target-")
}

/// Matches one path component using only `*` and `?`.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.as_bytes();
    let name = name.as_bytes();
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(at) = star {
            p = at + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct FakeEntry {
        dir: bool,
        symlink: bool,
        target: Option<PathBuf>,
        modified: SystemTime,
        len: u64,
    }

    #[derive(Default)]
    struct FakeFs {
        entries: Mutex<BTreeMap<PathBuf, FakeEntry>>,
        removed: Mutex<Vec<PathBuf>>,
    }

    impl FakeFs {
        fn ancestors(path: &Path) -> Vec<PathBuf> {
            let mut parents = Vec::new();
            let mut current = path.parent();
            while let Some(parent) = current {
                if parent.as_os_str().is_empty() {
                    break;
                }
                parents.push(parent.to_path_buf());
                current = parent.parent();
            }
            parents.reverse();
            parents
        }

        fn insert_dir(&self, path: &Path, modified: SystemTime) {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.entry(path.to_path_buf()).or_insert(FakeEntry {
                dir: true,
                symlink: false,
                target: None,
                modified,
                len: 0,
            });
        }

        fn dir(&self, path: &Path, modified: SystemTime) {
            for parent in Self::ancestors(path) {
                self.insert_dir(&parent, modified);
            }
            self.insert_dir(path, modified);
        }

        fn file(&self, path: &Path, len: u64, modified: SystemTime) {
            for parent in Self::ancestors(path) {
                self.insert_dir(&parent, modified);
            }
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.insert(
                path.to_path_buf(),
                FakeEntry {
                    dir: false,
                    symlink: false,
                    target: None,
                    modified,
                    len,
                },
            );
        }

        fn symlink(&self, path: &Path, target: &Path, modified: SystemTime) {
            for parent in Self::ancestors(path) {
                self.insert_dir(&parent, modified);
            }
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.insert(
                path.to_path_buf(),
                FakeEntry {
                    dir: false,
                    symlink: true,
                    target: Some(target.to_path_buf()),
                    modified,
                    len: 1,
                },
            );
        }

        fn entry(&self, path: &Path) -> io::Result<FakeEntry> {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing path"))
        }
    }

    impl FsOps for FakeFs {
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            let entry = self.entry(path)?;
            if entry.symlink {
                let target = entry.target.unwrap_or_default();
                return self.canonicalize(&target);
            }
            Ok(path.to_path_buf())
        }

        fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            let entry = self.entry(path)?;
            if !entry.dir || entry.symlink {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "not a directory",
                ));
            }
            Ok(self
                .entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .filter(|candidate| candidate.parent() == Some(path))
                .cloned()
                .collect())
        }

        fn symlink_metadata(&self, path: &Path) -> io::Result<FsMeta> {
            let entry = self.entry(path)?;
            Ok(FsMeta {
                is_dir: entry.dir,
                is_symlink: entry.symlink,
                modified: Some(entry.modified),
                len: entry.len,
            })
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            let entry = self.entry(path)?;
            if !entry.dir || entry.symlink {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "not a directory",
                ));
            }
            // Remove the tree without following links, then record the root the caller removed.
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let victims: Vec<PathBuf> = entries
                .keys()
                .filter(|candidate| *candidate == path || candidate.starts_with(path))
                .cloned()
                .collect();
            for victim in victims {
                entries.remove(&victim);
            }
            drop(entries);
            self.removed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(path.to_path_buf());
            Ok(())
        }
    }

    struct FakeTime {
        epoch: SystemTime,
        base: Instant,
        elapsed: Mutex<Duration>,
    }

    impl FakeTime {
        fn new() -> Self {
            Self {
                epoch: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                base: Instant::now() - Duration::from_secs(3600),
                elapsed: Mutex::new(Duration::ZERO),
            }
        }

        fn advance(&self, amount: Duration) {
            let mut elapsed = self.elapsed.lock().unwrap_or_else(|e| e.into_inner());
            *elapsed += amount;
        }
    }

    impl TimeSource for FakeTime {
        fn now(&self) -> SystemTime {
            let elapsed = self.elapsed.lock().unwrap_or_else(|e| e.into_inner());
            self.epoch + *elapsed
        }

        fn monotonic(&self) -> Instant {
            let elapsed = self.elapsed.lock().unwrap_or_else(|e| e.into_inner());
            self.base + *elapsed
        }
    }

    struct Fixture {
        fs: Arc<FakeFs>,
        time: Arc<FakeTime>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                fs: Arc::new(FakeFs::default()),
                time: Arc::new(FakeTime::new()),
            }
        }

        fn old(&self) -> SystemTime {
            self.time.now() - Duration::from_secs(10 * 3600)
        }

        fn make(&self, path: &str) {
            let path = Path::new(path);
            self.fs.dir(path, self.old());
            self.fs.file(&path.join("blob"), 8, self.old());
        }

        fn config(pattern: &str) -> HousekeepingConfig {
            HousekeepingConfig {
                scratch_globs: vec![pattern.to_owned()],
                extra_roots: Vec::new(),
                min_age_hours: 0,
                min_free_percent: 0,
                max_remove_per_pass: 10,
            }
        }

        fn house(&self, config: HousekeepingConfig) -> Housekeeper {
            Housekeeper::with_io(
                config,
                |_: &Path| {
                    Some(DiskSpace {
                        free_bytes: 0,
                        total_bytes: 100,
                    })
                },
                self.fs.clone(),
                self.time.clone(),
            )
        }

        fn removed(&self) -> Vec<PathBuf> {
            let mut paths = self.fs.removed.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *paths)
        }
    }

    fn free_at(percent: Arc<AtomicU64>) -> impl Fn(&Path) -> Option<DiskSpace> + Send + Sync {
        move |_: &Path| {
            Some(DiskSpace {
                free_bytes: percent.load(Ordering::Relaxed),
                total_bytes: 100,
            })
        }
    }

    #[test]
    fn removes_oldest_first() {
        let f = Fixture::new();
        f.make("/scratch/target-old");
        f.make("/scratch/target-new");
        f.fs.file(
            Path::new("/scratch/target-new/blob"),
            8,
            f.time.now() - Duration::from_secs(3600),
        );
        let mut config = Fixture::config("/scratch/*");
        config.max_remove_per_pass = 1;
        let mut house = f.house(config);
        let summary = house.run(Path::new("/work"), false, &[]).unwrap();
        assert_eq!(summary.removed, 1);
        assert!(f.removed().contains(&PathBuf::from("/scratch/target-old")));
    }

    #[test]
    fn respects_the_age_threshold() {
        let f = Fixture::new();
        f.make("/scratch/target-old");
        f.make("/scratch/target-young");
        f.fs.file(
            Path::new("/scratch/target-young/blob"),
            8,
            f.time.now() - Duration::from_secs(3600),
        );
        let mut config = Fixture::config("/scratch/*");
        config.min_age_hours = 6;
        let mut house = f.house(config);
        house.run(Path::new("/work"), false, &[]).unwrap();
        assert_eq!(f.removed(), [PathBuf::from("/scratch/target-old")]);
    }

    #[test]
    fn respects_the_free_percent_threshold() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        let mut config = Fixture::config("/scratch/*");
        config.min_free_percent = 15;
        let free = Arc::new(AtomicU64::new(30));
        let mut house = Housekeeper::with_io(
            config,
            free_at(Arc::clone(&free)),
            f.fs.clone(),
            f.time.clone(),
        );
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            0
        );
        assert!(f.removed().is_empty());

        free.store(10, Ordering::Relaxed);
        let mut config = Fixture::config("/scratch/*");
        config.min_free_percent = 15;
        let mut house = Housekeeper::with_io(config, free_at(free), f.fs.clone(), f.time.clone());
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            1
        );
    }

    #[test]
    fn zero_free_threshold_does_not_need_a_reading() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        let mut house = Housekeeper::with_io(
            Fixture::config("/scratch/*"),
            |_: &Path| None,
            f.fs.clone(),
            f.time.clone(),
        );
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            1
        );
    }

    #[test]
    fn stops_at_the_per_pass_cap() {
        let f = Fixture::new();
        for name in ["target-a", "target-b", "target-c"] {
            f.make(&format!("/scratch/{name}"));
        }
        let mut config = Fixture::config("/scratch/*");
        config.max_remove_per_pass = 2;
        let mut house = f.house(config);
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            2
        );
        assert_eq!(f.removed().len(), 2);
    }

    #[test]
    fn never_follows_a_symlink() {
        let f = Fixture::new();
        f.make("/elsewhere");
        f.fs.symlink(
            Path::new("/scratch/target-link"),
            Path::new("/elsewhere"),
            f.old(),
        );
        let mut house = f.house(Fixture::config("/scratch/*"));
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            0
        );
        assert!(f.removed().is_empty());
    }

    #[test]
    fn a_symlink_inside_a_candidate_is_not_followed() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        f.make("/elsewhere");
        f.fs.symlink(
            Path::new("/scratch/target-a/escape"),
            Path::new("/elsewhere"),
            f.old(),
        );
        let mut house = f.house(Fixture::config("/scratch/*"));
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            1
        );
        assert_eq!(f.removed(), [PathBuf::from("/scratch/target-a")]);
        assert!(!f
            .removed()
            .iter()
            .any(|path| path.starts_with("/elsewhere")));
    }

    #[test]
    fn refuses_roots_that_are_too_broad() {
        for wide in TOO_BROAD {
            let f = Fixture::new();
            let pattern = if wide == "/" {
                "/target-*".to_owned()
            } else {
                format!("{wide}/target-*")
            };
            let mut house = f.house(Fixture::config(&pattern));
            f.make(&format!("{}/target-a", wide.trim_end_matches('/')));
            assert_eq!(
                house.run(Path::new("/work"), false, &[]).unwrap().removed,
                0,
                "{wide}"
            );
            assert!(f.removed().is_empty());
        }
    }

    #[test]
    fn project_markers_stop_removal_outside_target_rules() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        f.fs.file(Path::new("/scratch/target-a/Cargo.toml"), 8, f.old());
        f.make("/scratch/target-b");
        f.fs.dir(Path::new("/scratch/target-b/.git"), f.old());
        let mut house = f.house(Fixture::config("/scratch/*"));
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            0
        );
        assert!(f.removed().is_empty());
    }

    #[test]
    fn target_rules_may_remove_their_own_build_output() {
        let f = Fixture::new();
        f.make("/scratch/target-debug");
        f.fs.file(Path::new("/scratch/target-debug/Cargo.toml"), 8, f.old());
        f.make("/scratch/target-release");
        f.fs.dir(Path::new("/scratch/target-release/.git"), f.old());
        let mut config = Fixture::config("/scratch/target-*");
        config.max_remove_per_pass = 2;
        let mut house = f.house(config);
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            2
        );
        assert_eq!(f.removed().len(), 2);
    }

    #[test]
    fn active_and_draining_workspaces_are_protected() {
        let f = Fixture::new();
        f.make("/scratch/job-active");
        f.make("/scratch/job-draining");
        let mut house = f.house(Fixture::config("/scratch/*"));
        let protected = [
            PathBuf::from("/scratch/job-active"),
            PathBuf::from("/scratch/job-draining"),
        ];
        assert_eq!(
            house
                .run(Path::new("/work"), false, &protected)
                .unwrap()
                .removed,
            0
        );
        assert!(f.removed().is_empty());
    }

    #[test]
    fn a_workspace_inside_a_candidate_protects_the_candidate() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        f.fs.dir(Path::new("/scratch/target-a/job"), f.time.now());
        let mut house = f.house(Fixture::config("/scratch/*"));
        assert_eq!(
            house
                .run(
                    Path::new("/work"),
                    false,
                    &[PathBuf::from("/scratch/target-a/job")],
                )
                .unwrap()
                .removed,
            0
        );
        assert!(f.removed().is_empty());
    }

    #[test]
    fn never_removes_the_work_directory() {
        let f = Fixture::new();
        f.make("/work/target-child");
        let mut house = f.house(Fixture::config("/work/*"));
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            0
        );
        assert!(f.removed().is_empty());
    }

    #[test]
    fn never_removes_files_even_when_they_match() {
        let f = Fixture::new();
        f.fs.file(Path::new("/scratch/target-file"), 8, f.old());
        let mut house = f.house(Fixture::config("/scratch/*"));
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            0
        );
        assert!(f.removed().is_empty());
    }

    #[test]
    fn nothing_runs_while_a_job_is_active() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        let mut house = f.house(Fixture::config("/scratch/*"));
        assert!(house.run(Path::new("/work"), true, &[]).is_none());
        assert!(f.removed().is_empty());
    }

    #[test]
    fn runs_at_most_once_every_ten_minutes() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        let mut config = Fixture::config("/scratch/*");
        config.max_remove_per_pass = 1;
        let mut house = f.house(config);
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            1
        );
        assert!(house.run(Path::new("/work"), false, &[]).is_none());
        f.time.advance(MIN_INTERVAL - Duration::from_secs(1));
        assert!(house.run(Path::new("/work"), false, &[]).is_none());
        f.time.advance(Duration::from_secs(2));
        let summary = house.run(Path::new("/work"), false, &[]).unwrap();
        assert_eq!(summary.removed, 0);
    }

    #[test]
    fn counters_accumulate_across_passes() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        let mut house = Housekeeper::with_io(
            Fixture::config("/scratch/*"),
            |_: &Path| None,
            f.fs.clone(),
            f.time.clone(),
        );
        let counters = house.counters();
        assert_eq!(counters.snapshot(), GcCounts::default());
        house.run(Path::new("/work"), false, &[]).unwrap();
        assert_eq!(
            counters.snapshot(),
            GcCounts {
                removed_total: 1,
                bytes_freed_total: 8
            }
        );
        f.time.advance(MIN_INTERVAL + Duration::from_secs(1));
        house.run(Path::new("/work"), false, &[]).unwrap();
        assert_eq!(counters.snapshot().removed_total, 1);
    }

    #[test]
    fn extra_roots_scan_their_target_children() {
        let f = Fixture::new();
        f.make("/host-scratch/target-debug");
        f.make("/host-scratch/other");
        let config = HousekeepingConfig {
            extra_roots: vec!["/host-scratch".to_owned()],
            ..Fixture::config("/unused/*")
        };
        let mut house = f.house(config);
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            1
        );
        assert_eq!(f.removed(), [PathBuf::from("/host-scratch/target-debug")]);
    }

    #[test]
    fn a_missing_parent_removes_nothing() {
        let f = Fixture::new();
        let mut house = f.house(Fixture::config("/missing/*"));
        let summary = house.run(Path::new("/work"), false, &[]).unwrap();
        assert_eq!(summary, PassSummary::default());
    }

    #[test]
    fn a_missing_free_space_reading_removes_nothing() {
        let f = Fixture::new();
        f.make("/scratch/target-a");
        let mut config = Fixture::config("/scratch/*");
        config.min_free_percent = 15;
        let mut house = Housekeeper::with_io(config, |_: &Path| None, f.fs.clone(), f.time.clone());
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            0
        );
        assert!(f.removed().is_empty());
    }

    #[test]
    fn glob_matches_only_its_component() {
        assert!(glob_matches("target-*", "target-debug"));
        assert!(glob_matches("target-?", "target-a"));
        assert!(!glob_matches("target-?", "target-ab"));
        assert!(glob_matches("*", "anything"));
        assert!(!glob_matches("target-*", "other"));
    }

    #[test]
    fn disk_space_reads_the_holding_filesystem() {
        let dir = std::env::temp_dir().join(format!("lu-housekeeping-disk-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let space = disk_space(&dir).unwrap();
        assert!(space.total_bytes > 0);
        assert!(space.free_bytes <= space.total_bytes);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn free_percent_handles_an_empty_filesystem() {
        assert_eq!(
            DiskSpace {
                free_bytes: 1,
                total_bytes: 0
            }
            .free_percent(),
            0
        );
    }

    #[test]
    fn housekeeper_ignores_a_pattern_without_a_parent() {
        let f = Fixture::new();
        let mut house = f.house(Fixture::config("/"));
        assert_eq!(
            house.run(Path::new("/work"), false, &[]).unwrap().removed,
            0
        );
        assert!(f.removed().is_empty());
    }
}
