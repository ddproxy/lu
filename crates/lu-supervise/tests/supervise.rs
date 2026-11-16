//! Real child processes: exit codes, signals, timeouts, process groups and output.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lu_supervise::{CommandSpec, Limits, Outcome, OutputChunk, Stream, Supervisor};

fn limits() -> Limits {
    Limits {
        wall_timeout: Duration::from_secs(30),
        idle_output_timeout: None,
        max_output_bytes: 64 * 1024 * 1024,
        grace: Duration::from_millis(200),
    }
}

fn sh(script: &str) -> CommandSpec {
    CommandSpec::new("sh").arg("-c").arg(script)
}

async fn drain(mut rx: tokio::sync::mpsc::Receiver<OutputChunk>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk.bytes);
    }
    out
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lu-supervise-{}-{}", std::process::id(), name))
}

fn wait_for_pid(path: &Path) -> u32 {
    for _ in 0..300 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no pid was written to {}", path.display());
}

/// True while `pid` is a live, non-zombie process.
///
/// A killed grandchild is reparented; when PID 1 does not reap, it lingers as a
/// zombie. A zombie is already terminated, so it counts as gone here.
fn pid_running(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match stat.rfind(')').and_then(|end| stat.get(end + 1..)) {
            Some(rest) => !rest.trim_start().starts_with('Z'),
            None => false,
        },
        Err(_) => false,
    }
}

fn wait_until_gone(pid: u32) -> bool {
    for _ in 0..300 {
        if !pid_running(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[tokio::test]
async fn a_normal_exit_reports_its_code() {
    let mut run = Supervisor::spawn(sh("exit 7"), limits());
    assert_eq!(run.wait().await, Outcome::Exited(7));
}

#[tokio::test]
async fn a_signal_death_reports_the_signal() {
    let mut run = Supervisor::spawn(sh("kill -TERM $$"), limits());
    assert_eq!(run.wait().await, Outcome::Signalled(15));
}

#[tokio::test]
async fn a_wall_timeout_ends_the_group() {
    let mut limits = limits();
    limits.wall_timeout = Duration::from_millis(200);
    let mut run = Supervisor::spawn(sh("sleep 300"), limits);
    assert_eq!(run.wait().await, Outcome::WallTimeout);
}

#[tokio::test]
async fn an_idle_timeout_ends_a_silent_child() {
    let mut limits = limits();
    limits.idle_output_timeout = Some(Duration::from_millis(200));
    let mut run = Supervisor::spawn(sh("sleep 300"), limits);
    assert_eq!(run.wait().await, Outcome::IdleTimeout);
}

#[tokio::test]
async fn output_keeps_the_idle_timeout_away() {
    let mut limits = limits();
    limits.idle_output_timeout = Some(Duration::from_millis(400));
    // Five ticks 100 ms apart span 500 ms with no gap longer than 100 ms.
    let script = "i=0; while [ $i -lt 5 ]; do echo tick; i=$((i+1)); sleep 0.1; done; exit 0";
    let mut run = Supervisor::spawn(sh(script), limits);
    assert_eq!(run.wait().await, Outcome::Exited(0));
}

#[tokio::test]
async fn too_much_output_ends_the_group() {
    let mut limits = limits();
    limits.max_output_bytes = 4096;
    let script = "while true; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done";
    let mut run = Supervisor::spawn(sh(script), limits);
    assert_eq!(run.wait().await, Outcome::OutputLimit);
}

#[tokio::test]
async fn stopping_ends_a_grandchild() {
    let pidfile = temp_path("grandchild.pid");
    let _ = std::fs::remove_file(&pidfile);
    let script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
    let mut run = Supervisor::spawn(sh(&script), limits());
    let pid = wait_for_pid(&pidfile);
    run.stop();
    assert_eq!(run.wait().await, Outcome::Stopped);
    assert!(wait_until_gone(pid), "grandchild {pid} is still alive");
    let _ = std::fs::remove_file(&pidfile);
}

#[tokio::test]
async fn a_child_that_ignores_terminate_needs_the_forced_stop() {
    let mut limits = limits();
    limits.grace = Duration::from_millis(300);
    let script = "trap '' TERM; while true; do sleep 1; done";
    let mut run = Supervisor::spawn(sh(script), limits);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    run.stop();
    assert_eq!(run.wait().await, Outcome::Stopped);
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "the grace period was not spent before the forced stop"
    );
}

#[tokio::test]
async fn dropping_running_ends_the_group() {
    let pidfile = temp_path("dropped.pid");
    let _ = std::fs::remove_file(&pidfile);
    let script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
    let run = Supervisor::spawn(sh(&script), limits());
    let pid = wait_for_pid(&pidfile);
    drop(run);
    assert!(wait_until_gone(pid), "grandchild {pid} outlived the handle");
    let _ = std::fs::remove_file(&pidfile);
}

#[tokio::test]
async fn closing_the_pipes_does_not_stop_the_child() {
    let mut limits = limits();
    limits.wall_timeout = Duration::from_millis(300);
    // Both pipes close at once; the child must keep running until the wall timeout,
    // not be mistaken for a stop request.
    let mut run = Supervisor::spawn(sh("exec 1>&- 2>&-; sleep 300"), limits);
    assert_eq!(run.wait().await, Outcome::WallTimeout);
}

#[tokio::test]
async fn a_slow_receiver_does_not_end_the_child() {
    // 32 * 64 KiB is more than the channel plus the pipe buffers hold.
    let script = "dd if=/dev/zero bs=65536 count=32 2>/dev/null";
    let mut run = Supervisor::spawn(sh(script), limits());
    let rx = run.output();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let bytes = drain(rx).await;
    assert_eq!(run.wait().await, Outcome::Exited(0));
    assert_eq!(bytes.len(), 32 * 65536);
}

#[tokio::test]
async fn a_dropped_receiver_does_not_stop_supervision() {
    let script = "dd if=/dev/zero bs=65536 count=32 2>/dev/null";
    let mut run = Supervisor::spawn(sh(script), limits());
    drop(run.output());
    assert_eq!(run.wait().await, Outcome::Exited(0));
}

#[tokio::test]
async fn a_missing_program_is_a_spawn_failure() {
    let mut run = Supervisor::spawn(
        CommandSpec::new("/nonexistent/lu-supervise-missing"),
        limits(),
    );
    match run.wait().await {
        Outcome::SpawnFailed(text) => assert!(!text.is_empty()),
        other => panic!("expected SpawnFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn stdout_and_stderr_are_merged_with_tags() {
    let mut run = Supervisor::spawn(sh("echo out; echo err 1>&2"), limits());
    let mut rx = run.output();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut saw_out, mut saw_err) = (false, false);
    while let Some(chunk) = rx.recv().await {
        match chunk.stream {
            Stream::Stdout => {
                saw_out = true;
                out.extend_from_slice(&chunk.bytes);
            }
            Stream::Stderr => {
                saw_err = true;
                err.extend_from_slice(&chunk.bytes);
            }
        }
    }
    assert_eq!(run.wait().await, Outcome::Exited(0));
    assert!(saw_out && saw_err, "both streams should be tagged");
    assert_eq!(out, b"out\n");
    assert_eq!(err, b"err\n");
}

#[tokio::test]
async fn the_child_sees_only_the_allowed_environment() {
    std::env::set_var("LU_SUPERVISE_TEST_PRIVATE", "topsecret");
    let mut run = Supervisor::spawn(sh("printf %s \"$LU_SUPERVISE_TEST_PRIVATE\""), limits());
    let bytes = drain(run.output()).await;
    assert_eq!(run.wait().await, Outcome::Exited(0));
    assert!(bytes.is_empty(), "private variable leaked: {bytes:?}");
}

#[tokio::test]
async fn a_given_environment_entry_is_visible() {
    let spec = CommandSpec::new("sh")
        .arg("-c")
        .arg("printf %s \"$LU_GIVEN\"")
        .env("LU_GIVEN", "yes");
    let mut run = Supervisor::spawn(spec, limits());
    let bytes = drain(run.output()).await;
    assert_eq!(run.wait().await, Outcome::Exited(0));
    assert_eq!(bytes, b"yes");
}

#[tokio::test]
async fn path_is_passed_through() {
    let mut run = Supervisor::spawn(sh("test -n \"$PATH\""), limits());
    assert_eq!(run.wait().await, Outcome::Exited(0));
}
