//! Unit tests for the agent loop. Uses a scripted `Controller` and a `sh -c` harness.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use lu_client::{ApiError, Controller, PollRequest};
use lu_proto::{AgentEvent, ArtifactName, Completion, JobId, JobSpec, LogChunk, NodeName};
use lu_staging::Staging;
use lu_supervise::CommandSpec;
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::{Agent, AgentError};

const TEST_HEARTBEAT_SECS: u64 = 5;

/// A recorded call to the fake controller.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Poll(PollRequest),
    Start {
        job_id: JobId,
        epoch: u64,
    },
    Renew {
        job_id: JobId,
        epoch: u64,
    },
    AppendLog(LogChunk),
    PutArtifact {
        job_id: JobId,
        epoch: u64,
        name: ArtifactName,
        bytes: Vec<u8>,
    },
    Complete(Completion),
}

/// A fake controller that records calls and returns scripted replies.
struct FakeController {
    calls: Mutex<Vec<Call>>,
    poll_replies: Mutex<VecDeque<Result<Vec<AgentEvent>, ApiError>>>,
    start_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    renew_replies: Mutex<VecDeque<Result<bool, ApiError>>>,
    append_log_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    put_artifact_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    complete_replies: Mutex<VecDeque<Result<(), ApiError>>>,
}

impl FakeController {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            poll_replies: Mutex::new(VecDeque::new()),
            start_replies: Mutex::new(VecDeque::new()),
            renew_replies: Mutex::new(VecDeque::new()),
            append_log_replies: Mutex::new(VecDeque::new()),
            put_artifact_replies: Mutex::new(VecDeque::new()),
            complete_replies: Mutex::new(VecDeque::new()),
        }
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    fn take_calls(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    fn push_poll(&self, reply: Result<Vec<AgentEvent>, ApiError>) {
        self.poll_replies.lock().unwrap().push_back(reply);
    }

    fn push_start(&self, reply: Result<(), ApiError>) {
        self.start_replies.lock().unwrap().push_back(reply);
    }

    fn push_renew(&self, reply: Result<bool, ApiError>) {
        self.renew_replies.lock().unwrap().push_back(reply);
    }

    fn push_append_log(&self, reply: Result<(), ApiError>) {
        self.append_log_replies.lock().unwrap().push_back(reply);
    }

    fn push_put_artifact(&self, reply: Result<(), ApiError>) {
        self.put_artifact_replies.lock().unwrap().push_back(reply);
    }

    fn push_complete(&self, reply: Result<(), ApiError>) {
        self.complete_replies.lock().unwrap().push_back(reply);
    }

    fn pop_poll(&self) -> Option<Result<Vec<AgentEvent>, ApiError>> {
        self.poll_replies.lock().unwrap().pop_front()
    }

    fn pop_start(&self) -> Option<Result<(), ApiError>> {
        self.start_replies.lock().unwrap().pop_front()
    }

    fn pop_renew(&self) -> Option<Result<bool, ApiError>> {
        self.renew_replies.lock().unwrap().pop_front()
    }

    fn pop_append_log(&self) -> Option<Result<(), ApiError>> {
        self.append_log_replies.lock().unwrap().pop_front()
    }

    fn pop_put_artifact(&self) -> Option<Result<(), ApiError>> {
        self.put_artifact_replies.lock().unwrap().pop_front()
    }

    fn pop_complete(&self) -> Option<Result<(), ApiError>> {
        self.complete_replies.lock().unwrap().pop_front()
    }
}

impl Controller for FakeController {
    fn poll<'a>(
        &'a self,
        request: &'a PollRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<AgentEvent>, ApiError>> + Send + 'a>> {
        let request = request.clone();
        self.record(Call::Poll(request));
        Box::pin(async move { self.pop_poll().unwrap_or(Ok(Vec::new())) })
    }

    fn start<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        let job_id = id.clone();
        self.record(Call::Start {
            job_id: job_id.clone(),
            epoch,
        });
        Box::pin(async move { self.pop_start().unwrap_or(Ok(())) })
    }

    fn renew<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>> {
        let job_id = id.clone();
        self.record(Call::Renew {
            job_id: job_id.clone(),
            epoch,
        });
        Box::pin(async move { self.pop_renew().unwrap_or(Ok(false)) })
    }

    fn append_log<'a>(
        &'a self,
        chunk: &'a LogChunk,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        let chunk = chunk.clone();
        self.record(Call::AppendLog(chunk));
        Box::pin(async move { self.pop_append_log().unwrap_or(Ok(())) })
    }

    fn put_artifact<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
        name: ArtifactName,
        bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        let job_id = id.clone();
        let bytes = bytes.to_vec();
        self.record(Call::PutArtifact {
            job_id: job_id.clone(),
            epoch,
            name,
            bytes,
        });
        Box::pin(async move { self.pop_put_artifact().unwrap_or(Ok(())) })
    }

    fn complete<'a>(
        &'a self,
        completion: &'a Completion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        let completion = completion.clone();
        self.record(Call::Complete(completion));
        Box::pin(async move { self.pop_complete().unwrap_or(Ok(())) })
    }
}

/// Builds a minimal valid JobSpec for testing.
fn test_spec(repo: &str) -> JobSpec {
    JobSpec {
        repo: repo.to_owned(),
        git_ref: "main".to_owned(),
        task: "test task".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 60,
        idle_timeout_secs: None,
        idempotency_key: None,
    }
}

/// Creates an agent with the fake controller and a scripted harness command.
fn make_agent(
    work_dir: &std::path::Path,
    controller: Arc<FakeController>,
    harness_script: impl Fn(&std::path::Path) -> CommandSpec + Send + Sync + 'static,
) -> Agent {
    let staging = Staging::new(work_dir).unwrap();
    let node = NodeName::parse("node-a").unwrap();
    Agent {
        node,
        controller,
        staging,
        work_dir: work_dir.to_path_buf(),
        heartbeat_secs: TEST_HEARTBEAT_SECS,
        opencode_server: None,
        harness: "opencode".to_owned(),
        builder: Arc::new(move |task_file, _model, _server, _message| {
            Ok(harness_script(task_file))
        }),
    }
}

/// A harness that exits 0 immediately.
fn success_harness(_task_file: &std::path::Path) -> CommandSpec {
    CommandSpec::new("sh").arg("-c").arg("echo success; exit 0")
}

/// A harness that exits with a non-zero code.
fn fail_harness(_task_file: &std::path::Path) -> CommandSpec {
    CommandSpec::new("sh").arg("-c").arg("echo failure; exit 1")
}

/// A harness that prints the provider-unavailable marker and exits non-zero.
/// A harness that prints a large amount of output (exceeds log cap).
/// A harness that writes the environment to stdout.
/// A harness that runs for a while (used for shutdown test).
fn slow_harness(_task_file: &std::path::Path) -> CommandSpec {
    CommandSpec::new("sh").arg("-c").arg("sleep 10; echo done")
}

/// Creates a local git repository for testing.
fn create_git_repo(path: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(path)
        .status()
        .unwrap();
    std::fs::write(path.join("README.md"), "test\n").unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=test@test",
            "-c",
            "user.name=test",
            "add",
            "README.md",
        ])
        .current_dir(path)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=test@test",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "init",
        ])
        .current_dir(path)
        .status()
        .unwrap();
}

fn run_agent_with_shutdown(
    agent: Agent,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), AgentError>>,
) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });
    (shutdown_tx, handle)
}

/// Helper to create a fake controller with a single Assign event in the poll reply.
fn controller_with_assign(job_id: JobId, epoch: u64, spec: JobSpec) -> Arc<FakeController> {
    let controller = Arc::new(FakeController::new());
    controller.push_poll(Ok(vec![AgentEvent::Assign {
        job_id: job_id.clone(),
        epoch,
        spec,
    }]));
    controller.push_start(Ok(()));
    controller.push_renew(Ok(false)); // First renew returns false (no cancel)
    controller.push_complete(Ok(()));
    controller
}

#[tokio::test]
async fn renew_true_stops_harness_no_completion() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let epoch = 1;
    let spec = test_spec(&repo_dir.to_string_lossy());

    let controller = controller_with_assign(job_id.clone(), epoch, spec.clone());
    // First renew returns true (cancel requested)
    controller.push_renew(Ok(true));

    let agent = make_agent(&work_dir, controller.clone(), slow_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok(), "agent run failed: {result:?}");

    let calls = controller.take_calls();
    // Should have poll, start, renew (cancel=true), but NO complete
    let complete_calls: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, Call::Complete(_)))
        .collect();
    assert_eq!(
        complete_calls.len(),
        0,
        "no completion should be sent on cancel"
    );
}

#[tokio::test]
async fn stale_epoch_from_any_lease_call_stops_job_without_completing() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let epoch = 1;
    let spec = test_spec(&repo_dir.to_string_lossy());

    let controller = Arc::new(FakeController::new());
    controller.push_poll(Ok(vec![AgentEvent::Assign {
        job_id: job_id.clone(),
        epoch,
        spec: spec.clone(),
    }]));
    // start returns StaleEpoch
    controller.push_start(Err(ApiError::StaleEpoch));

    let agent = make_agent(&work_dir, controller.clone(), success_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok(), "agent run failed: {result:?}");

    let calls = controller.take_calls();
    // Should have poll, start, but NO complete
    let complete_calls: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, Call::Complete(_)))
        .collect();
    assert_eq!(complete_calls.len(), 0);
}

#[tokio::test]
async fn workspace_removed_after_success() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let epoch = 1;
    let spec = test_spec(&repo_dir.to_string_lossy());

    let controller = controller_with_assign(job_id.clone(), epoch, spec.clone());
    controller.push_append_log(Ok(()));
    controller.push_put_artifact(Ok(()));
    controller.push_put_artifact(Ok(()));

    let agent = make_agent(&work_dir, controller.clone(), success_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok());

    // Workspace directory should be removed
    let job_workspace = work_dir.join(job_id.as_str());
    assert!(
        !job_workspace.exists(),
        "workspace should be removed after success"
    );
}

#[tokio::test]
async fn workspace_removed_after_failure() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let epoch = 1;
    let spec = test_spec(&repo_dir.to_string_lossy());

    let controller = controller_with_assign(job_id.clone(), epoch, spec.clone());
    controller.push_append_log(Ok(()));
    controller.push_put_artifact(Ok(()));
    controller.push_put_artifact(Ok(()));

    let agent = make_agent(&work_dir, controller.clone(), fail_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok());

    let job_workspace = work_dir.join(job_id.as_str());
    assert!(
        !job_workspace.exists(),
        "workspace should be removed after failure"
    );
}

#[tokio::test]
async fn workspace_removed_after_cancel() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let epoch = 1;
    let spec = test_spec(&repo_dir.to_string_lossy());

    let controller = controller_with_assign(job_id.clone(), epoch, spec.clone());
    controller.push_renew(Ok(true)); // cancel requested

    let agent = make_agent(&work_dir, controller.clone(), slow_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok());

    let job_workspace = work_dir.join(job_id.as_str());
    assert!(
        !job_workspace.exists(),
        "workspace should be removed after cancel"
    );
}

/// Tests that the supervisor's OutputLimit outcome is correctly classified.
/// This is marked ignore because the current implementation may not hit OutputLimit
/// in the way we expect with the test harness.
#[tokio::test]
async fn poll_cancel_event_for_non_running_job_ignored() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let controller = Arc::new(FakeController::new());
    let job_id = JobId::new();
    controller.push_poll(Ok(vec![AgentEvent::Cancel {
        job_id: job_id.clone(),
        epoch: 1,
    }]));
    // Then shutdown
    controller.push_poll(Err(ApiError::Network {
        message: "shutdown".to_owned(),
    }));

    let agent = make_agent(&work_dir, controller.clone(), success_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn poll_followup_event_ignored() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let controller = Arc::new(FakeController::new());
    let job_id = JobId::new();
    controller.push_poll(Ok(vec![AgentEvent::Followup {
        job_id: job_id.clone(),
        epoch: 1,
        message: "followup".to_owned(),
    }]));
    controller.push_poll(Err(ApiError::Network {
        message: "shutdown".to_owned(),
    }));

    let agent = make_agent(&work_dir, controller.clone(), success_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn control_loop_cancel_event_stops_job() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let epoch = 1;
    let spec = test_spec(&repo_dir.to_string_lossy());

    let controller = Arc::new(FakeController::new());
    controller.push_poll(Ok(vec![AgentEvent::Assign {
        job_id: job_id.clone(),
        epoch,
        spec: spec.clone(),
    }]));
    controller.push_start(Ok(()));
    // Heartbeat poll returns Cancel for the job
    controller.push_poll(Ok(vec![AgentEvent::Cancel {
        job_id: job_id.clone(),
        epoch,
    }]));
    controller.push_renew(Ok(false));
    controller.push_append_log(Ok(()));

    let agent = make_agent(&work_dir, controller.clone(), slow_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok());

    let calls = controller.take_calls();
    // No completion should be sent for cancelled job
    let complete_calls: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, Call::Complete(_)))
        .collect();
    assert_eq!(complete_calls.len(), 0);
}

#[tokio::test]
async fn control_loop_stale_epoch_stops_job() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();

    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let epoch = 1;
    let spec = test_spec(&repo_dir.to_string_lossy());

    let controller = Arc::new(FakeController::new());
    controller.push_poll(Ok(vec![AgentEvent::Assign {
        job_id: job_id.clone(),
        epoch,
        spec: spec.clone(),
    }]));
    controller.push_start(Ok(()));
    // Renew returns StaleEpoch
    controller.push_renew(Err(ApiError::StaleEpoch));

    let agent = make_agent(&work_dir, controller.clone(), slow_harness);
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    assert!(result.is_ok());

    let calls = controller.take_calls();
    let complete_calls: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, Call::Complete(_)))
        .collect();
    assert_eq!(complete_calls.len(), 0);
}

#[tokio::test]
async fn upload_loop_conflict_resumes_without_duplicate_or_skip() {
    // This test verifies the log upload loop's conflict handling directly
    use super::logs::upload_loop;

    use lu_proto::JobId;
    use tokio::sync::{mpsc, oneshot};

    let controller = Arc::new(FakeController::new());
    let job_id = JobId::new();
    let epoch = 1;

    // First append succeeds at offset 0
    controller.push_append_log(Ok(()));
    // Second append returns Conflict with length 100
    controller.push_append_log(Err(ApiError::Conflict { length: 100 }));
    // Third append at offset 100 succeeds
    controller.push_append_log(Ok(()));

    let (tx, rx) = mpsc::channel(10);
    let (_stop_tx, stop_rx) = oneshot::channel();
    let (fault_tx, _fault_rx) = mpsc::channel(1);

    // Send two chunks: first 50 bytes, second 60 bytes
    tx.send(vec![b'x'; 50]).await.unwrap();
    tx.send(vec![b'y'; 60]).await.unwrap();
    drop(tx);

    let upload = upload_loop(controller.clone(), job_id, epoch, rx, stop_rx, fault_tx).await;

    assert!(upload.fault.is_none());
    let calls = controller.take_calls();
    let log_calls: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, Call::AppendLog(_)))
        .collect();
    assert_eq!(log_calls.len(), 3);

    // First chunk at offset 0
    if let Call::AppendLog(c) = &log_calls[0] {
        assert_eq!(c.seq, 0);
        assert_eq!(c.data.len(), 50);
    }
    // Second chunk at offset 50
    if let Call::AppendLog(c) = &log_calls[1] {
        assert_eq!(c.seq, 50);
        assert_eq!(c.data.len(), 60);
    }
    // After conflict (length=100), third call at offset 100 with remaining 10 bytes
    if let Call::AppendLog(c) = &log_calls[2] {
        assert_eq!(c.seq, 100);
        assert_eq!(c.data.len(), 10); // 110 total - 100 = 10 remaining
    }
}

#[tokio::test]
async fn upload_loop_stale_epoch_returns_fault() {
    use super::logs::upload_loop;

    use lu_proto::JobId;
    use tokio::sync::{mpsc, oneshot};

    let controller = Arc::new(FakeController::new());
    let job_id = JobId::new();
    let epoch = 1;

    controller.push_append_log(Err(ApiError::StaleEpoch));

    let (tx, rx) = mpsc::channel(10);
    let (_stop_tx, stop_rx) = oneshot::channel();
    let (fault_tx, mut fault_rx) = mpsc::channel(1);

    tx.send(vec![b'x'; 10]).await.unwrap();
    drop(tx);

    let upload = upload_loop(controller.clone(), job_id, epoch, rx, stop_rx, fault_tx).await;

    assert!(matches!(upload.fault, Some(super::UploadFault::Stale)));
    let fault = fault_rx.recv().await.unwrap();
    assert!(matches!(fault, super::UploadFault::Stale));
}

#[tokio::test]
async fn upload_loop_unauthorized_returns_fault() {
    use super::logs::upload_loop;

    use lu_proto::JobId;
    use tokio::sync::{mpsc, oneshot};

    let controller = Arc::new(FakeController::new());
    let job_id = JobId::new();
    let epoch = 1;

    controller.push_append_log(Err(ApiError::Unauthorized));

    let (tx, rx) = mpsc::channel(10);
    let (_stop_tx, stop_rx) = oneshot::channel();
    let (fault_tx, _fault_rx) = mpsc::channel(1);

    tx.send(vec![b'x'; 10]).await.unwrap();
    drop(tx);

    let upload = upload_loop(controller.clone(), job_id, epoch, rx, stop_rx, fault_tx).await;

    assert!(matches!(
        upload.fault,
        Some(super::UploadFault::Unauthorized)
    ));
}

#[tokio::test]
async fn upload_loop_forbidden_returns_fault() {
    use super::logs::upload_loop;

    use lu_proto::JobId;
    use tokio::sync::{mpsc, oneshot};

    let controller = Arc::new(FakeController::new());
    let job_id = JobId::new();
    let epoch = 1;

    controller.push_append_log(Err(ApiError::Forbidden));

    let (tx, rx) = mpsc::channel(10);
    let (_stop_tx, stop_rx) = oneshot::channel();
    let (fault_tx, _fault_rx) = mpsc::channel(1);

    tx.send(vec![b'x'; 10]).await.unwrap();
    drop(tx);

    let upload = upload_loop(controller.clone(), job_id, epoch, rx, stop_rx, fault_tx).await;

    assert!(matches!(upload.fault, Some(super::UploadFault::Forbidden)));
}

#[tokio::test]
async fn split_lossy_handles_invalid_utf8() {
    use super::logs::split_lossy;

    let bytes = b"hello\xff\xfe world";
    let chunks = split_lossy(bytes);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].contains("hello"));
    assert!(chunks[0].contains("world"));
    // Invalid bytes should become replacement characters
    assert!(chunks[0].contains('\u{FFFD}'));
}

#[tokio::test]
async fn summarise_caps_at_max_summary_bytes() {
    use lu_proto::MAX_SUMMARY_BYTES;

    let long_text = "x".repeat(MAX_SUMMARY_BYTES + 100);
    let summary = super::summarise(&long_text);
    assert!(summary.len() <= MAX_SUMMARY_BYTES);
}

#[tokio::test]
async fn provider_marker_detects_various_patterns() {
    assert!(super::provider_marker(
        "every key in this stream's chain is unavailable"
    ));
    assert!(super::provider_marker("QUOTA exceeded"));
    assert!(super::provider_marker("HTTP 429"));
    assert!(super::provider_marker("429 Too Many Requests"));
    assert!(super::provider_marker("rate limit hit"));
    assert!(!super::provider_marker("normal output"));
}
