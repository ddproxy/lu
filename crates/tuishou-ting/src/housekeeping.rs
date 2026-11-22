//! Opt-in removal of leftover scratch directories between jobs.
//!
//! A pass only looks at direct children of a configured pattern's parent and only removes
//! regular directories. Everything else, including symlinks and the agent's work directory, is
//! left alone. Any error ends that rule's pass instead of failing the agent.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use lu_config::HousekeepingConfig;
use tracing::{info, warn};

/// Runs before the first poll and after each job, but no more often than this.
pub const MIN_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Extra headroom above `min_free_percent` that a removal pass aims for.
const TARGET_HEADROOM_PERCENT: u64 = 5;

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
}

/// Stateful housekeeping for one agent. `run` is a no-op while a job is active or within
/// [`MIN_INTERVAL`] of the previous pass.
pub struct Housekeeper {
    rules: Vec<Rule>,
    min_age_hours: u64,
    min_free_percent: u64,
    max_remove_per_pass: usize,
    free_space: Arc<FreeSpaceFn>,
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
        let rules = config
            .scratch_globs
            .iter()
            .filter_map(|glob| {
                let path = Path::new(glob);
                Some(Rule {
                    parent: path.parent()?.to_path_buf(),
                    pattern: path.file_name()?.to_string_lossy().into_owned(),
                })
            })
            .collect();
        Self {
            rules,
            min_age_hours: config.min_age_hours,
            min_free_percent: config.min_free_percent,
            max_remove_per_pass: config.max_remove_per_pass,
            free_space: Arc::new(free_space),
            last_pass: None,
        }
    }

    /// Runs a pass when due. `None` means it was skipped because a job is active or the last
    /// pass was too recent.
    pub fn run(&mut self, work_dir: &Path, job_running: bool) -> Option<PassSummary> {
        if job_running {
            return None;
        }
        let now = Instant::now();
        if self
            .last_pass
            .is_some_and(|last| now.duration_since(last) < MIN_INTERVAL)
        {
            return None;
        }
        self.last_pass = Some(now);
        Some(self.pass(work_dir))
    }

    fn pass(&self, work_dir: &Path) -> PassSummary {
        let work_canon = fs::canonicalize(work_dir).ok();
        let mut summary = PassSummary::default();
        'rules: for rule in &self.rules {
            let Ok(parent) = fs::canonicalize(&rule.parent) else {
                continue;
            };
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
            let mut candidates = self.candidates(rule, &parent, work_canon.as_deref());
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
                match fs::remove_dir_all(&candidate.canonical) {
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

    fn candidates(&self, rule: &Rule, parent: &Path, work_canon: Option<&Path>) -> Vec<Candidate> {
        let now = SystemTime::now();
        let Ok(entries) = fs::read_dir(parent) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !glob_matches(&rule.pattern, name) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_dir() || meta.file_type().is_symlink() {
                continue;
            }
            let Ok(canonical) = fs::canonicalize(&path) else {
                continue;
            };
            if canonical.parent() != Some(parent) {
                continue;
            }
            if work_canon.is_some_and(|work| canonical == work || canonical.starts_with(work)) {
                continue;
            }
            let Some((newest, size_bytes)) = scan_tree(&path) else {
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
}

struct Candidate {
    path: PathBuf,
    canonical: PathBuf,
    newest: SystemTime,
    size_bytes: u64,
    age: Duration,
}

/// Newest modification time and apparent size of a directory tree, without following links.
/// `None` means part of the tree could not be read, so the entry is not safe to remove.
fn scan_tree(root: &Path) -> Option<(SystemTime, u64)> {
    let mut newest = None;
    let mut size_bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path).ok()?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if let Ok(modified) = meta.modified() {
            newest = Some(match newest {
                Some(current) if current >= modified => current,
                _ => modified,
            });
        }
        if meta.is_dir() {
            for entry in fs::read_dir(&path).ok()? {
                let entry = entry.ok()?;
                stack.push(entry.path());
            }
        } else {
            size_bytes = size_bytes.saturating_add(meta.len());
        }
    }
    Some((newest?, size_bytes))
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_config(pattern: &str) -> HousekeepingConfig {
        HousekeepingConfig {
            scratch_globs: vec![pattern.to_owned()],
            min_age_hours: 0,
            min_free_percent: 0,
            max_remove_per_pass: 10,
        }
    }

    fn scratch(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "lu-housekeeping-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn work_dir(root: &Path) -> PathBuf {
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        work
    }

    fn always_free(_: &Path) -> Option<DiskSpace> {
        Some(DiskSpace {
            free_bytes: 0,
            total_bytes: 100,
        })
    }

    fn make(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("file"), b"x").unwrap();
    }

    fn set_age(path: &Path, secs: u64) {
        let modified = SystemTime::now() - Duration::from_secs(secs) - Duration::from_secs(60 * 60);
        let mut stack = vec![path.to_path_buf()];
        while let Some(path) = stack.pop() {
            let file = fs::File::open(&path).unwrap();
            file.set_modified(modified).unwrap();
            if path.is_dir() {
                for entry in fs::read_dir(&path).unwrap() {
                    stack.push(entry.unwrap().path());
                }
            }
        }
    }

    #[test]
    fn removes_oldest_first() {
        let root = scratch("oldest");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        make(&parent.join("target-old"));
        make(&parent.join("target-new"));
        set_age(&parent.join("target-old"), 10 * 3600);
        set_age(&parent.join("target-new"), 3600);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.max_remove_per_pass = 1;
        let mut house = Housekeeper::with_free_space(config, always_free);
        let summary = house.run(&work, false).unwrap();
        assert_eq!(summary.removed, 1);
        assert!(!parent.join("target-old").exists());
        assert!(parent.join("target-new").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn respects_the_age_threshold() {
        let root = scratch("age");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        make(&parent.join("target-old"));
        make(&parent.join("target-young"));
        set_age(&parent.join("target-old"), 8 * 3600);
        set_age(&parent.join("target-young"), 2 * 3600);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_age_hours = 6;
        let mut house = Housekeeper::with_free_space(config, always_free);
        house.run(&work, false).unwrap();
        assert!(!parent.join("target-old").exists());
        assert!(parent.join("target-young").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn respects_the_free_percent_threshold() {
        let root = scratch("free");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        make(&parent.join("target-a"));
        set_age(&parent.join("target-a"), 10 * 3600);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_free_percent = 15;
        let free = Arc::new(std::sync::atomic::AtomicU64::new(30));
        let injected = {
            let free = Arc::clone(&free);
            move |_: &Path| {
                Some(DiskSpace {
                    free_bytes: free.load(Ordering::Relaxed),
                    total_bytes: 100,
                })
            }
        };
        let mut house = Housekeeper::with_free_space(config, injected);
        assert_eq!(house.run(&work, false).unwrap().removed, 0);
        assert!(parent.join("target-a").exists());

        free.store(10, Ordering::Relaxed);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_free_percent = 15;
        let injected = {
            let free = Arc::clone(&free);
            move |_: &Path| {
                Some(DiskSpace {
                    free_bytes: free.load(Ordering::Relaxed),
                    total_bytes: 100,
                })
            }
        };
        let mut house = Housekeeper::with_free_space(config, injected);
        assert_eq!(house.run(&work, false).unwrap().removed, 1);
        assert!(!parent.join("target-a").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stops_at_the_per_pass_cap() {
        let root = scratch("cap");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        for name in ["target-a", "target-b", "target-c"] {
            make(&parent.join(name));
            set_age(&parent.join(name), 10 * 3600);
        }
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.max_remove_per_pass = 2;
        let mut house = Housekeeper::with_free_space(config, always_free);
        let summary = house.run(&work, false).unwrap();
        assert_eq!(summary.removed, 2);
        let left = ["target-a", "target-b", "target-c"]
            .iter()
            .filter(|name| parent.join(name).exists())
            .count();
        assert_eq!(left, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn never_follows_a_symlink_to_another_directory() {
        let root = scratch("symlink");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&parent).unwrap();
        make(&elsewhere);
        std::os::unix::fs::symlink(&elsewhere, parent.join("target-link")).unwrap();
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, always_free);
        let summary = house.run(&work, false).unwrap();
        assert_eq!(summary.removed, 0);
        assert!(elsewhere.exists());
        assert!(parent.join("target-link").symlink_metadata().is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn never_removes_the_work_directory_or_its_contents() {
        let root = scratch("work");
        let work = work_dir(&root);
        make(&work.join("target-child"));
        set_age(&work.join("target-child"), 10 * 3600);
        set_age(&work, 10 * 3600);
        let mut config = make_config(&format!("{}/*", root.display()));
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, always_free);
        let summary = house.run(&work, false).unwrap();
        assert_eq!(summary.removed, 0);
        assert!(work.exists());
        assert!(work.join("target-child").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn never_removes_files_even_when_they_match() {
        let root = scratch("files");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        fs::write(parent.join("target-file"), b"x").unwrap();
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, always_free);
        assert_eq!(house.run(&work, false).unwrap().removed, 0);
        assert!(parent.join("target-file").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nothing_runs_while_a_job_is_active() {
        let root = scratch("busy");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        make(&parent.join("target-a"));
        set_age(&parent.join("target-a"), 10 * 3600);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, always_free);
        assert!(house.run(&work, true).is_none());
        assert!(parent.join("target-a").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runs_at_most_once_every_ten_minutes() {
        let root = scratch("interval");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        make(&parent.join("target-old"));
        make(&parent.join("target-new"));
        set_age(&parent.join("target-old"), 10 * 3600);
        set_age(&parent.join("target-new"), 10 * 3600);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.max_remove_per_pass = 1;
        let mut house = Housekeeper::with_free_space(config, always_free);
        assert_eq!(house.run(&work, false).unwrap().removed, 1);
        assert!(house.run(&work, false).is_none());
        let left = fs::read_dir(&parent).unwrap().count();
        assert_eq!(left, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn errors_are_swallowed() {
        let root = scratch("errors");
        let work = work_dir(&root);
        let missing = root.join("missing");
        let mut config = make_config(&format!("{}/*", missing.display()));
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, always_free);
        let summary = house.run(&work, false).unwrap();
        assert_eq!(summary.removed, 0);
        assert_eq!(summary.failed, 0);

        let file = root.join("not-a-directory");
        fs::write(&file, b"x").unwrap();
        let mut config = make_config(&format!("{}/*", file.display()));
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, always_free);
        assert_eq!(house.run(&work, false).unwrap().removed, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_free_space_reading_removes_nothing() {
        let root = scratch("nospace");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        make(&parent.join("target-a"));
        set_age(&parent.join("target-a"), 10 * 3600);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_age_hours = 0;
        config.min_free_percent = 15;
        let mut house = Housekeeper::with_free_space(config, |_: &Path| None);
        assert_eq!(house.run(&work, false).unwrap().removed, 0);
        assert!(parent.join("target-a").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn zero_free_threshold_does_not_need_a_reading() {
        let root = scratch("nospace-zero");
        let work = work_dir(&root);
        let parent = root.join("scratch");
        fs::create_dir_all(&parent).unwrap();
        make(&parent.join("target-a"));
        set_age(&parent.join("target-a"), 10 * 3600);
        let mut config = make_config(&format!("{}/*", parent.display()));
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, |_: &Path| None);
        assert_eq!(house.run(&work, false).unwrap().removed, 1);
        assert!(!parent.join("target-a").exists());
        let _ = fs::remove_dir_all(root);
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
        let root = scratch("disk");
        let space = disk_space(&root).unwrap();
        assert!(space.total_bytes > 0);
        assert!(space.free_bytes <= space.total_bytes);
        let _ = fs::remove_dir_all(root);
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
        let root = scratch("noparent");
        let work = work_dir(&root);
        let mut config = make_config("/");
        config.min_age_hours = 0;
        let mut house = Housekeeper::with_free_space(config, always_free);
        assert_eq!(house.run(&work, false).unwrap().removed, 0);
        let _ = fs::remove_dir_all(root);
    }
}
