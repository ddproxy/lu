//! Operator-declared label probes. Each probe runs in its own task, off the heartbeat path;
//! heartbeats read only the last result.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lu_config::DetectConfig;
use lu_proto::printable;
use lu_supervise::{CommandSpec, Limits, Outcome as ProbeOutcome, Supervisor};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

/// Time between terminate and forced stop; bounded so a stuck probe cannot hold a heartbeat.
const GRACE: Duration = Duration::from_millis(500);
const MAX_PROBE_OUTPUT: u64 = 4096;

/// The last published state of every probe label.
#[derive(Default)]
pub(crate) struct ProbeResults {
    published: Mutex<BTreeSet<String>>,
}

impl ProbeResults {
    pub(crate) fn published(&self) -> BTreeSet<String> {
        self.published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns true only when the label's presence changed.
    pub(crate) fn set(&self, label: &str, on: bool) -> bool {
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if on {
            published.insert(label.to_owned())
        } else {
            published.remove(label)
        }
    }
}

pub(crate) struct Probes {
    entries: Vec<DetectConfig>,
    results: Arc<ProbeResults>,
}

impl Probes {
    pub(crate) fn new(entries: Vec<DetectConfig>) -> Self {
        Self {
            entries,
            results: Arc::new(ProbeResults::default()),
        }
    }

    pub(crate) fn results(&self) -> Arc<ProbeResults> {
        Arc::clone(&self.results)
    }

    /// Starts one task per probe. The caller owns the handles and aborts them on shutdown.
    pub(crate) fn spawn_all(&self) -> Vec<JoinHandle<()>> {
        self.entries
            .iter()
            .cloned()
            .map(|entry| spawn_one(entry, Arc::clone(&self.results)))
            .collect()
    }
}

fn spawn_one(entry: DetectConfig, results: Arc<ProbeResults>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let running = Arc::new(AtomicBool::new(false));
        let mut tick = interval_at(Instant::now(), Duration::from_secs(entry.every_secs.max(1)));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // A slow probe skips its next tick instead of running two copies at once.
            if running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            run_once(&entry, &results).await;
            running.store(false, Ordering::SeqCst);
        }
    })
}

async fn run_once(entry: &DetectConfig, results: &ProbeResults) {
    debug!(label = %printable(&entry.label), "running label probe");
    let spec = CommandSpec::new(entry.command[0].clone())
        .args(entry.command[1..].iter().map(String::as_str));
    let limits = Limits {
        wall_timeout: Duration::from_secs(entry.timeout_secs),
        idle_output_timeout: None,
        max_output_bytes: MAX_PROBE_OUTPUT,
        grace: GRACE,
    };
    let mut running = Supervisor::spawn(spec, limits);
    // The supervisor kills the group on its own timeout; this only guards against it hanging.
    let outer = Duration::from_secs(entry.timeout_secs) + GRACE + Duration::from_secs(1);
    let outcome = match timeout(outer, running.wait()).await {
        Ok(outcome) => outcome,
        Err(_) => ProbeOutcome::WallTimeout,
    };
    let on = matches!(outcome, ProbeOutcome::Exited(0));
    debug!(label = %printable(&entry.label), outcome = ?outcome, "label probe finished");
    if results.set(&entry.label, on) {
        if on {
            info!(label = %printable(&entry.label), "probe published node label");
        } else {
            warn!(label = %printable(&entry.label), "probe stopped publishing node label");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn detect(
        program: &str,
        args: &[&str],
        label: &str,
        timeout_secs: u64,
        every_secs: u64,
    ) -> DetectConfig {
        let mut command = vec![program.to_owned()];
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        DetectConfig {
            label: label.to_owned(),
            command,
            timeout_secs,
            every_secs,
        }
    }

    async fn wait_until<F: Fn() -> bool>(limit: Duration, ready: F) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < limit {
            if ready() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ready()
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn a_successful_probe_publishes_its_label() {
        let probes = Probes::new(vec![detect("/bin/true", &[], "php8.3", 2, 1)]);
        let results = probes.results();
        let tasks = probes.spawn_all();
        assert!(
            wait_until(Duration::from_secs(3), || results
                .published()
                .contains("php8.3"))
            .await
        );
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_failed_probe_removes_its_label() {
        let probes = Probes::new(vec![detect("/bin/false", &[], "gpu", 2, 60)]);
        let results = probes.results();
        assert!(results.set("gpu", true));
        let tasks = probes.spawn_all();
        assert!(
            wait_until(Duration::from_secs(3), || !results
                .published()
                .contains("gpu"))
            .await
        );
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_spawn_failure_removes_its_label() {
        let probes = Probes::new(vec![detect(
            "/definitely/not/a/lu-probe",
            &[],
            "broken",
            2,
            60,
        )]);
        let results = probes.results();
        assert!(results.set("broken", true));
        let tasks = probes.spawn_all();
        assert!(
            wait_until(Duration::from_secs(3), || !results
                .published()
                .contains("broken"))
            .await
        );
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_timed_out_probe_removes_its_label() {
        let probes = Probes::new(vec![detect("/bin/sleep", &["30"], "slow", 1, 60)]);
        let results = probes.results();
        assert!(results.set("slow", true));
        let tasks = probes.spawn_all();
        assert!(
            wait_until(Duration::from_secs(5), || !results
                .published()
                .contains("slow"))
            .await
        );
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn probes_run_again_at_the_configured_interval() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("runs");
        let shell = format!("echo run >> '{}'", marker.display());
        let probes = Probes::new(vec![detect("/bin/sh", &["-c", &shell], "interval", 2, 1)]);
        let tasks = probes.spawn_all();
        assert!(wait_until(Duration::from_secs(4), || lines(&marker).len() >= 2).await);
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_slow_probe_never_overlaps_itself() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("runs");
        let shell = format!(
            "echo start >> '{f}'; sleep 1; echo end >> '{f}'",
            f = marker.display()
        );
        let probes = Probes::new(vec![detect("/bin/sh", &["-c", &shell], "slow", 5, 1)]);
        let tasks = probes.spawn_all();
        assert!(
            wait_until(Duration::from_secs(8), || lines(&marker)
                .iter()
                .filter(|line| line.as_str() == "end")
                .count()
                >= 2)
            .await
        );
        for task in tasks {
            task.abort();
        }
        let seen = lines(&marker);
        // The last run may have been aborted between its start and end markers.
        let complete = seen.len() / 2 * 2;
        assert!(complete >= 4, "{seen:?}");
        for pair in seen[..complete].chunks(2) {
            assert_eq!(pair, &["start", "end"]);
        }
    }
}
