//! Linux `/proc` sampling for host and job observability.
//!
//! Everything here is best-effort: an unreadable file becomes an omitted value or a zero, never
//! an error that could disturb a running job. Other platforms report nothing. No new dependency
//! and no `unsafe` is used; the disk fields come from the housekeeping helper.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use lu_proto::{printable, truncate_chars, HostStats, JobId, JobStats, ProcessStats};

/// A running job is stuck when neither its harness output nor its process tree has moved for
/// longer than this. This is the observation threshold; the policy adds a separate continuous
/// interval before the agent acts.
pub const STUCK_AFTER_SECS: u64 = 300;

/// The clock used by stuck detection. Kept injectable so tests can advance idle time without
/// sleeping for the production thresholds.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The wall clock used in production.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The most processes one job reports. A busy harness can start hundreds; the observer keeps
/// the ones that have used the most CPU.
pub const MAX_PROCESSES: usize = 64;

/// The command line quoted in a process entry.
pub const CMDLINE_CHARS: usize = 80;

/// `/proc/<pid>/stat` reports CPU time in clock ticks, and Linux's user-space clock is 100 Hz.
const CLOCK_TICKS_PER_SEC: u64 = 100;

/// Whether this build samples `/proc` at all.
pub fn enabled() -> bool {
    cfg!(target_os = "linux")
}

/// Samples the real host. `None` on a non-Linux build or when the required files cannot be read.
/// The disk numbers describe the filesystem holding `disk_root`, when it can be read.
pub fn host_stats(disk_root: &Path) -> Option<HostStats> {
    #[cfg(target_os = "linux")]
    {
        let mut host = host_stats_at(Path::new("/proc"))?;
        if let Some(space) = crate::housekeeping::disk_space(disk_root) {
            host.disk_free_bytes = Some(space.free_bytes);
            host.disk_total_bytes = Some(space.total_bytes);
        }
        Some(host)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = disk_root;
        None
    }
}

/// Reads host numbers from a `/proc`-shaped root. Disk fields stay `None`; [`host_stats`] fills
/// them from the real filesystem.
pub fn host_stats_at(root: &Path) -> Option<HostStats> {
    let load = parse_loadavg(&std::fs::read_to_string(root.join("loadavg")).ok()?)?;
    let (mem_total_kb, mem_available_kb, swap_used_kb) =
        parse_meminfo(&std::fs::read_to_string(root.join("meminfo")).ok()?)?;
    let uptime_secs = parse_uptime(&std::fs::read_to_string(root.join("uptime")).ok()?)?;
    Some(HostStats {
        load1: load[0],
        load5: load[1],
        load15: load[2],
        mem_total_kb,
        mem_available_kb,
        swap_used_kb,
        disk_free_bytes: None,
        disk_total_bytes: None,
        uptime_secs,
    })
}

fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut fields = text.split_whitespace();
    let load1 = fields.next()?.parse().ok()?;
    let load5 = fields.next()?.parse().ok()?;
    let load15 = fields.next()?.parse().ok()?;
    Some([load1, load5, load15])
}

fn parse_uptime(text: &str) -> Option<u64> {
    let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(seconds as u64)
}

fn parse_meminfo(text: &str) -> Option<(u64, u64, u64)> {
    let mut total = None;
    let mut available = None;
    let mut free = None;
    let mut swap_total = 0;
    let mut swap_free = 0;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        match key {
            "MemTotal" => total = Some(value),
            "MemAvailable" => available = Some(value),
            "MemFree" => free = Some(value),
            "SwapTotal" => swap_total = value,
            "SwapFree" => swap_free = value,
            _ => {}
        }
    }
    let total = total?;
    // Older kernels have no MemAvailable; MemFree is a conservative stand-in.
    let available = available.or(free).unwrap_or(0);
    Some((total, available, swap_total.saturating_sub(swap_free)))
}

/// The fields of `/proc/<pid>/stat` this module uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatFields {
    pid: u32,
    state: String,
    ppid: u32,
    pgrp: u32,
    cpu_secs: u64,
    start_ticks: u64,
}

/// Parses one line of `/proc/<pid>/stat`. The command name is in parentheses and may itself
/// contain spaces or parentheses, so the last `)` is the only safe end marker.
fn parse_stat(text: &str) -> Option<StatFields> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid = text.get(..open)?.trim().parse().ok()?;
    let tokens: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    let state = tokens.first()?.to_string();
    let ppid = tokens.get(1)?.parse().ok()?;
    let pgrp = tokens.get(2)?.parse().ok()?;
    // Tokens after the command name start at stat field 3.
    let utime: u64 = tokens.get(11)?.parse().ok()?;
    let stime: u64 = tokens.get(12)?.parse().ok()?;
    let start_ticks: u64 = tokens.get(19)?.parse().ok()?;
    Some(StatFields {
        pid,
        state,
        ppid,
        pgrp,
        cpu_secs: utime.saturating_add(stime) / CLOCK_TICKS_PER_SEC,
        start_ticks,
    })
}

#[derive(Debug, Clone)]
struct ProcEntry {
    pid: u32,
    ppid: u32,
    pgrp: u32,
    state: String,
    cpu_secs: u64,
    rss_kb: u64,
    age_secs: u64,
    cmdline: String,
    open_files: u32,
}

/// The processes in one harness tree, plus the uncapped CPU total used for liveness.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SampledTree {
    pub processes: Vec<ProcessStats>,
    pub total_cpu_secs: u64,
}

/// Samples the real `/proc` tree for `pgid`. The process list is busiest-first and capped.
pub fn sample_tree_at(root: &Path, pgid: u32, uptime_secs: u64) -> SampledTree {
    build_tree(read_entries(root, uptime_secs), pgid)
}

fn read_entries(root: &Path, uptime_secs: u64) -> Vec<ProcEntry> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|entry| read_entry(&entry.path(), uptime_secs))
        .collect()
}

fn read_entry(dir: &Path, uptime_secs: u64) -> Option<ProcEntry> {
    let pid = dir.file_name()?.to_str()?.parse::<u32>().ok()?;
    let stat = parse_stat(&std::fs::read_to_string(dir.join("stat")).ok()?)?;
    if stat.pid != pid {
        return None;
    }
    Some(ProcEntry {
        pid,
        ppid: stat.ppid,
        pgrp: stat.pgrp,
        state: stat.state,
        cpu_secs: stat.cpu_secs,
        rss_kb: read_rss_kb(dir),
        age_secs: uptime_secs.saturating_sub(stat.start_ticks / CLOCK_TICKS_PER_SEC),
        cmdline: read_cmdline(dir),
        open_files: read_open_files(dir),
    })
}

fn read_rss_kb(dir: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(dir.join("status")) else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

fn read_open_files(dir: &Path) -> u32 {
    std::fs::read_dir(dir.join("fd"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
        .try_into()
        .unwrap_or(u32::MAX)
}

fn read_cmdline(dir: &Path) -> String {
    let bytes = std::fs::read(dir.join("cmdline")).unwrap_or_default();
    cmdline_text(&bytes)
}

/// Flags whose next argument is a secret.
const SECRET_FLAGS: &[&str] = &[
    "--password",
    "--passwd",
    "--token",
    "--secret",
    "--key",
    "--api-key",
    "--apikey",
    "--auth",
    "-p",
];
/// Prefixes of well-known credential formats.
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "xox",
    "AKIA",
];

fn secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|word| upper.contains(word))
}

/// A long mixed letter and digit string that is not a path is treated as a credential.
fn token_like(arg: &str) -> bool {
    arg.len() >= 24
        && !arg.starts_with(['/', '.', '-'])
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'=' | b'.'))
        && arg.bytes().any(|b| b.is_ascii_digit())
        && arg.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Replaces the arguments that look like secrets with `***`.
fn redact_args(args: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for arg in args {
        if hide_next {
            hide_next = false;
            out.push("***".to_owned());
            continue;
        }
        if SECRET_FLAGS.contains(arg) {
            hide_next = true;
            out.push((*arg).to_owned());
            continue;
        }
        if let Some((name, _)) = arg.split_once('=') {
            if secret_name(name.trim_start_matches('-')) {
                out.push(format!("{name}=***"));
                continue;
            }
        }
        if SECRET_PREFIXES.iter().any(|prefix| arg.starts_with(prefix)) || token_like(arg) {
            out.push("***".to_owned());
            continue;
        }
        out.push((*arg).to_owned());
    }
    out
}

/// Turns a NUL-separated command line into one redacted, safe, capped line.
fn cmdline_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let args: Vec<&str> = text.split('\0').filter(|arg| !arg.is_empty()).collect();
    let text = redact_args(&args).join(" ");
    truncate_chars(&printable(&text), CMDLINE_CHARS)
}

fn build_tree(entries: Vec<ProcEntry>, pgid: u32) -> SampledTree {
    let mut by_pid: BTreeMap<u32, ProcEntry> = entries.into_iter().map(|e| (e.pid, e)).collect();
    let mut tree: BTreeSet<u32> = by_pid
        .values()
        .filter(|entry| entry.pgrp == pgid)
        .map(|entry| entry.pid)
        .collect();
    // A descendant that called setpgid leaves the group but is still part of the tree.
    loop {
        let mut added = false;
        for (pid, entry) in &by_pid {
            if !tree.contains(pid) && tree.contains(&entry.ppid) {
                tree.insert(*pid);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    let mut processes: Vec<ProcessStats> = tree
        .iter()
        .filter_map(|pid| by_pid.remove(pid))
        .map(|entry| ProcessStats {
            pid: entry.pid,
            ppid: entry.ppid,
            state: entry.state,
            cpu_secs: entry.cpu_secs,
            rss_kb: entry.rss_kb,
            age_secs: entry.age_secs,
            cmdline: entry.cmdline,
            open_files: entry.open_files,
        })
        .collect();
    let total_cpu_secs = processes.iter().map(|process| process.cpu_secs).sum();
    processes.sort_by(|a, b| b.cpu_secs.cmp(&a.cpu_secs).then(a.pid.cmp(&b.pid)));
    processes.truncate(MAX_PROCESSES);
    SampledTree {
        processes,
        total_cpu_secs,
    }
}

/// A job is stuck only while it is running and both kinds of idleness exceed the threshold.
#[cfg(test)]
pub fn is_stuck(running: bool, output_idle_secs: u64, cpu_idle_secs: u64) -> bool {
    is_stuck_at(running, output_idle_secs, cpu_idle_secs, STUCK_AFTER_SECS)
}

/// The same test as [`is_stuck`] with an explicit observation threshold.
pub fn is_stuck_at(
    running: bool,
    output_idle_secs: u64,
    cpu_idle_secs: u64,
    threshold_secs: u64,
) -> bool {
    running && output_idle_secs > threshold_secs && cpu_idle_secs > threshold_secs
}

/// Tracks the last time the tree's CPU total changed.
#[derive(Debug, Clone)]
pub struct CpuIdle {
    total_cpu_secs: u64,
    since: Instant,
}

impl CpuIdle {
    pub fn new(now: Instant) -> Self {
        Self {
            total_cpu_secs: 0,
            since: now,
        }
    }

    /// Records a sample. Any change, including a lower total after a restart, counts as activity.
    pub fn observe(&mut self, total_cpu_secs: u64, now: Instant) {
        if total_cpu_secs != self.total_cpu_secs {
            self.total_cpu_secs = total_cpu_secs;
            self.since = now;
        }
    }

    pub fn idle_secs(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.since).as_secs()
    }
}

/// The mutable state one running job keeps between heartbeats.
#[derive(Debug, Clone)]
pub struct JobObservation {
    job_id: JobId,
    pgid: u32,
    last_output: Instant,
    cpu: CpuIdle,
    detect_secs: u64,
}

impl JobObservation {
    #[cfg(test)]
    pub fn new(job_id: JobId, pgid: u32, now: Instant) -> Self {
        Self::new_with_detect_secs(job_id, pgid, now, STUCK_AFTER_SECS)
    }

    /// Builds an observation with a custom idle threshold. Tests lower it so idle jobs can be
    /// classified without waiting five minutes.
    pub fn new_with_detect_secs(job_id: JobId, pgid: u32, now: Instant, detect_secs: u64) -> Self {
        Self {
            job_id,
            pgid,
            last_output: now,
            cpu: CpuIdle::new(now),
            detect_secs,
        }
    }

    pub fn record_output(&mut self, now: Instant) {
        self.last_output = now;
    }

    /// Builds one job's stats. `running` is false only if the job has stopped being observed.
    pub fn sample(
        &mut self,
        root: &Path,
        uptime_secs: u64,
        running: bool,
        now: Instant,
    ) -> JobStats {
        let tree = if enabled() {
            sample_tree_at(root, self.pgid, uptime_secs)
        } else {
            SampledTree::default()
        };
        self.cpu.observe(tree.total_cpu_secs, now);
        let output_idle_secs = now.saturating_duration_since(self.last_output).as_secs();
        let cpu_idle_secs = self.cpu.idle_secs(now);
        JobStats {
            job_id: self.job_id.clone(),
            pgid: self.pgid,
            output_idle_secs,
            cpu_idle_secs,
            stuck: is_stuck_at(running, output_idle_secs, cpu_idle_secs, self.detect_secs),
            processes: tree.processes,
        }
    }
}

/// Counts how long a job has been reported stuck without interruption. Any non-stuck sample
/// clears the interval; reaching `after_secs` returns the elapsed time for the completion summary.
#[derive(Debug, Clone, Default)]
pub struct StuckTracker {
    since: Option<Instant>,
}

impl StuckTracker {
    /// Records one sample. `Some(secs)` means the continuous threshold was reached on this sample.
    pub fn observe(&mut self, stuck: bool, now: Instant, after_secs: u64) -> Option<u64> {
        if !stuck {
            self.since = None;
            return None;
        }
        let since = *self.since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(since).as_secs();
        (elapsed >= after_secs).then_some(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "lu-proc-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn write_process(
        root: &Path,
        pid: u32,
        ppid: u32,
        pgrp: u32,
        utime: u64,
        stime: u64,
        start: u64,
        rss_kb: u64,
        cmdline: &[u8],
    ) {
        let dir = root.join(pid.to_string());
        write_file(
            &dir.join("stat"),
            format!(
                "{pid} (comm {pid}) S {ppid} {pgrp} 0 0 0 0 0 0 0 0 {utime} {stime} 0 0 20 0 1 0 {start} 0 0"
            )
            .as_bytes(),
        );
        write_file(
            &dir.join("status"),
            format!("Name:\tprocess\nVmRSS:\t{rss_kb} kB\n").as_bytes(),
        );
        write_file(&dir.join("cmdline"), cmdline);
        write_file(&dir.join("fd").join("0"), b"");
    }

    #[test]
    fn stat_parses_a_comm_with_spaces_and_parentheses() {
        let text = "4242 (a b) c) S 1 4242 4242 0 -1 4194560 100 0 0 0 300 200 0 0 20 0 1 0 500 1000000 250";
        let stat = parse_stat(text).unwrap();
        assert_eq!(stat.pid, 4242);
        assert_eq!(stat.state, "S");
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.pgrp, 4242);
        assert_eq!(stat.cpu_secs, 5);
        assert_eq!(stat.start_ticks, 500);
    }

    #[test]
    fn host_numbers_come_from_a_fake_proc_root() {
        let root = temp_root("host");
        write_file(&root.join("loadavg"), b"0.50 1.25 2.00 1/100 123\n");
        write_file(
            &root.join("meminfo"),
            b"MemTotal:       16384 kB\nMemFree:         2048 kB\nMemAvailable:    8192 kB\nSwapTotal:       4096 kB\nSwapFree:        1024 kB\n",
        );
        write_file(&root.join("uptime"), b"1234.56 987.00\n");
        let host = host_stats_at(&root).unwrap();
        assert_eq!(host.load1, 0.5);
        assert_eq!(host.load5, 1.25);
        assert_eq!(host.load15, 2.0);
        assert_eq!(host.mem_total_kb, 16_384);
        assert_eq!(host.mem_available_kb, 8_192);
        assert_eq!(host.swap_used_kb, 3_072);
        assert_eq!(host.uptime_secs, 1_234);
        assert!(host.disk_free_bytes.is_none());
        assert!(host.disk_total_bytes.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_proc_root_reports_nothing() {
        assert!(host_stats_at(Path::new("/definitely/not/a/proc")).is_none());
        assert_eq!(
            sample_tree_at(Path::new("/definitely/not/a/proc"), 1, 0),
            SampledTree::default()
        );
    }

    #[test]
    fn the_tree_keeps_group_members_and_descendants_but_not_unrelated_processes() {
        let root = temp_root("tree");
        write_process(&root, 500, 1, 500, 100, 100, 100, 64, b"harness\0run\0");
        write_process(&root, 501, 500, 500, 200, 0, 50, 32, b"child");
        // A descendant in its own group still belongs to the tree.
        write_process(&root, 502, 501, 999, 300, 0, 10, 16, b"grandchild");
        write_process(&root, 600, 1, 600, 9_999, 0, 0, 8, b"unrelated");
        let tree = sample_tree_at(&root, 500, 200);
        let mut pids: Vec<u32> = tree.processes.iter().map(|p| p.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, vec![500, 501, 502]);
        assert_eq!(tree.total_cpu_secs, 7);
        let harness = tree.processes.iter().find(|p| p.pid == 500).unwrap();
        assert_eq!(harness.rss_kb, 64);
        assert_eq!(harness.age_secs, 199);
        assert_eq!(harness.open_files, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_process_list_is_busiest_first_and_capped() {
        let root = temp_root("cap");
        for pid in 1..=70 {
            write_process(&root, pid, 1, 700, pid as u64 * 100, 0, 0, 0, b"x");
        }
        let tree = sample_tree_at(&root, 700, 0);
        assert_eq!(tree.processes.len(), MAX_PROCESSES);
        assert_eq!(tree.processes[0].pid, 70);
        assert_eq!(tree.processes[0].cpu_secs, 70);
        assert_eq!(tree.total_cpu_secs, (1..=70).sum::<u64>());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn command_lines_are_redacted_and_capped() {
        let long = format!("a\0b\u{1b}[31m{}", "x".repeat(200));
        let text = cmdline_text(long.as_bytes());
        assert!(text.starts_with("a b?[31m"), "{text}");
        assert_eq!(text.chars().count(), CMDLINE_CHARS);
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn secrets_in_command_lines_are_replaced() {
        let line = |parts: &[&str]| cmdline_text(parts.join("\0").as_bytes());
        assert_eq!(
            line(&["curl", "--token", "abc123", "url"]),
            "curl --token *** url"
        );
        assert_eq!(line(&["tool", "API_KEY=abc", "x"]), "tool API_KEY=*** x");
        assert_eq!(
            line(&["git", "clone", "ghp_abcdefghijklmnop"]),
            "git clone ***"
        );
        assert_eq!(
            line(&["run", "9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c"]),
            "run ***"
        );
        assert_eq!(
            line(&[
                "cargo",
                "test",
                "--workspace",
                "/srv/work/lu/abc123def456ghi789jkl"
            ]),
            "cargo test --workspace /srv/work/lu/abc123def456ghi789jkl"
        );
        assert_eq!(
            line(&["opencode", "run", "--auto", "-m", "x/y"]),
            "opencode run --auto -m x/y"
        );
    }

    #[test]
    fn stuck_needs_a_running_job_and_both_idle_times_over_the_threshold() {
        assert!(!is_stuck(false, 301, 301));
        assert!(!is_stuck(true, 300, 301));
        assert!(!is_stuck(true, 301, 300));
        assert!(is_stuck(true, 301, 301));
    }

    #[test]
    fn cpu_idle_resets_only_when_the_total_changes() {
        let now = Instant::now();
        let mut idle = CpuIdle::new(now);
        idle.observe(5, now + Duration::from_secs(10));
        assert_eq!(idle.idle_secs(now + Duration::from_secs(320)), 310);
        idle.observe(5, now + Duration::from_secs(320));
        assert_eq!(idle.idle_secs(now + Duration::from_secs(321)), 311);
        idle.observe(6, now + Duration::from_secs(321));
        assert_eq!(idle.idle_secs(now + Duration::from_secs(321)), 0);
    }

    #[test]
    fn output_and_cpu_idleness_together_mark_a_job_stuck() {
        let root = temp_root("stuck");
        write_process(&root, 900, 1, 900, 0, 0, 0, 1, b"harness");
        let now = Instant::now();
        let mut observation = JobObservation::new(JobId::new(), 900, now);
        observation.record_output(now + Duration::from_secs(20));
        let job = observation.sample(&root, 1_000, true, now + Duration::from_secs(321));
        assert_eq!(job.output_idle_secs, 301);
        assert_eq!(job.cpu_idle_secs, 321);
        assert!(job.stuck);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_custom_threshold_changes_classification() {
        assert!(!is_stuck_at(true, 10, 11, 10));
        assert!(!is_stuck_at(true, 11, 10, 10));
        assert!(is_stuck_at(true, 11, 11, 10));
        assert!(is_stuck_at(true, 11, 11, 5));
        let root = temp_root("stuck-threshold");
        write_process(&root, 901, 1, 901, 0, 0, 0, 1, b"harness");
        let now = Instant::now();
        let mut observation = JobObservation::new_with_detect_secs(JobId::new(), 901, now, 5);
        let job = observation.sample(&root, 1_000, true, now + Duration::from_secs(11));
        assert!(job.stuck);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tracker_counts_continuous_stuck_time_and_resets_on_activity() {
        let now = Instant::now();
        let mut tracker = StuckTracker::default();
        assert_eq!(tracker.observe(false, now, 5), None);
        assert_eq!(tracker.observe(true, now, 5), None);
        assert_eq!(tracker.observe(true, now + Duration::from_secs(4), 5), None);
        assert_eq!(
            tracker.observe(true, now + Duration::from_secs(5), 5),
            Some(5)
        );

        let mut tracker = StuckTracker::default();
        tracker.observe(true, now, 5);
        assert_eq!(
            tracker.observe(false, now + Duration::from_secs(4), 5),
            None
        );
        assert_eq!(tracker.observe(true, now + Duration::from_secs(8), 5), None);
        assert_eq!(
            tracker.observe(true, now + Duration::from_secs(13), 5),
            Some(5)
        );
    }
}
