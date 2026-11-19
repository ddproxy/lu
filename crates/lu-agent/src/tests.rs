//! Unit tests for the agent loop. Uses a scripted `Controller` and a `sh -c` harness.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lu_client::{ApiError, Controller, DependencyView, PollRequest};
use lu_config::BaseSource;
use lu_proto::{
    AgentEvent, ArtifactName, Completion, FailureClass, JobId, JobSpec, JobState, LogChunk,
    NodeName, Outcome, MAX_LOG_BYTES,
};
use lu_staging::Staging;
use lu_supervise::CommandSpec;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::{Agent, AgentError};

const TEST_HEARTBEAT_SECS: u64 = 5;
const IDLE_POLL: Duration = Duration::from_millis(10);
const JOB_TIMEOUT: Duration = Duration::from_secs(30);
const EPOCH: u64 = 1;

type BaseReply = Result<Option<(String, Vec<u8>)>, ApiError>;

/// A recorded call to the fake controller.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Poll(PollRequest),
    JobBase {
        job_id: JobId,
        epoch: u64,
    },
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
    Deps {
        job_id: JobId,
        epoch: u64,
    },
    DepDiff {
        job_id: JobId,
        dep: JobId,
        epoch: u64,
    },
    Complete(Completion),
}

/// A fake controller that records calls and returns scripted replies.
struct FakeController {
    calls: Mutex<Vec<Call>>,
    poll_replies: Mutex<VecDeque<Result<Vec<AgentEvent>, ApiError>>>,
    base_replies: Mutex<VecDeque<BaseReply>>,
    start_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    renew_replies: Mutex<VecDeque<Result<bool, ApiError>>>,
    append_log_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    put_artifact_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    deps_replies: Mutex<VecDeque<Result<Vec<DependencyView>, ApiError>>>,
    dep_diff_replies: Mutex<VecDeque<Result<Vec<u8>, ApiError>>>,
    complete_replies: Mutex<VecDeque<Result<(), ApiError>>>,
}

impl FakeController {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            poll_replies: Mutex::new(VecDeque::new()),
            base_replies: Mutex::new(VecDeque::new()),
            start_replies: Mutex::new(VecDeque::new()),
            renew_replies: Mutex::new(VecDeque::new()),
            append_log_replies: Mutex::new(VecDeque::new()),
            put_artifact_replies: Mutex::new(VecDeque::new()),
            deps_replies: Mutex::new(VecDeque::new()),
            dep_diff_replies: Mutex::new(VecDeque::new()),
            complete_replies: Mutex::new(VecDeque::new()),
        }
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    fn take_calls(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    /// Runs `f` over the recorded calls without taking them.
    fn check<R>(&self, f: impl FnOnce(&[Call]) -> R) -> R {
        f(&self.calls.lock().unwrap())
    }

    fn push_poll(&self, reply: Result<Vec<AgentEvent>, ApiError>) {
        self.poll_replies.lock().unwrap().push_back(reply);
    }

    fn push_base(&self, reply: Result<Option<(String, Vec<u8>)>, ApiError>) {
        self.base_replies.lock().unwrap().push_back(reply);
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

    fn push_deps(&self, reply: Result<Vec<DependencyView>, ApiError>) {
        self.deps_replies.lock().unwrap().push_back(reply);
    }

    fn push_dep_diff(&self, reply: Result<Vec<u8>, ApiError>) {
        self.dep_diff_replies.lock().unwrap().push_back(reply);
    }

    fn push_complete(&self, reply: Result<(), ApiError>) {
        self.complete_replies.lock().unwrap().push_back(reply);
    }

    fn pop_poll(&self) -> Option<Result<Vec<AgentEvent>, ApiError>> {
        self.poll_replies.lock().unwrap().pop_front()
    }

    fn pop_base(&self) -> Option<BaseReply> {
        self.base_replies.lock().unwrap().pop_front()
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

    fn pop_deps(&self) -> Option<Result<Vec<DependencyView>, ApiError>> {
        self.deps_replies.lock().unwrap().pop_front()
    }

    fn pop_dep_diff(&self) -> Option<Result<Vec<u8>, ApiError>> {
        self.dep_diff_replies.lock().unwrap().pop_front()
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
        // An empty poll sleeps so the run loop yields; a ready future would spin a
        // current-thread runtime forever once the scripted replies run out.
        Box::pin(async move {
            match self.pop_poll() {
                Some(reply) => reply,
                None => {
                    tokio::time::sleep(IDLE_POLL).await;
                    Ok(Vec::new())
                }
            }
        })
    }

    fn job_base<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>, ApiError>> + Send + 'a>>
    {
        let job_id = id.clone();
        self.record(Call::JobBase {
            job_id: job_id.clone(),
            epoch,
        });
        Box::pin(async move { self.pop_base().unwrap_or(Ok(None)) })
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

    fn deps<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DependencyView>, ApiError>> + Send + 'a>> {
        let job_id = id.clone();
        self.record(Call::Deps {
            job_id: job_id.clone(),
            epoch,
        });
        Box::pin(async move { self.pop_deps().unwrap_or(Ok(Vec::new())) })
    }

    fn dep_diff<'a>(
        &'a self,
        id: &'a JobId,
        dep: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ApiError>> + Send + 'a>> {
        let job_id = id.clone();
        let dep = dep.clone();
        self.record(Call::DepDiff {
            job_id: job_id.clone(),
            dep: dep.clone(),
            epoch,
        });
        Box::pin(async move { self.pop_dep_diff().unwrap_or(Ok(Vec::new())) })
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
        base_commit: None,
        task: "test task".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 60,
        idle_timeout_secs: None,
        idempotency_key: None,
        after: Vec::new(),
        stack_dependencies: true,
        allow_empty: false,
        requires: Vec::new(),
        fallbacks: Vec::new(),
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
        base_source: BaseSource::Auto,
        work_dir: work_dir.to_path_buf(),
        heartbeat_secs: TEST_HEARTBEAT_SECS,
        labels: Arc::new(super::labels::LabelSource::new(Vec::new(), Vec::new())),
        provider_marker_grace: super::PROVIDER_MARKER_GRACE,
        reiki_watch_period: super::REIKI_WATCH_PERIOD,
        opencode_server: None,
        workspace_gid: None,
        opencode_password: None,
        reiki_probe: None,
        health: Default::default(),
        harness: "opencode".to_owned(),
        harness_wrapper: Vec::new(),
        builder: Arc::new(move |task_file, _model, _server, _message| {
            Ok(harness_script(task_file))
        }),
        housekeeper: None,
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

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// Creates a local git repository for testing.
fn create_git_repo(path: &Path) {
    git(path, &["init", "-q", "-b", "main"]);
    git(
        path,
        &["config", "--local", "user.email", "test@example.net"],
    );
    git(path, &["config", "--local", "user.name", "Test"]);
    git(path, &["config", "--local", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "test\n").unwrap();
    git(path, &["add", "README.md"]);
    git(path, &["commit", "-q", "-m", "init"]);
}

/// A patch from the repository's committed `before` content to `after`, with the worktree
/// restored afterwards so several dependencies can be generated against the same base.
fn make_diff(repo_dir: &Path, path: &str, before: &str, after: &str) -> Vec<u8> {
    std::fs::write(repo_dir.join(path), after).unwrap();
    let output = std::process::Command::new("git")
        .args([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--binary",
            "HEAD",
            "--",
            path,
        ])
        .current_dir(repo_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(repo_dir.join(path), before).unwrap();
    output.stdout
}

fn dependency(id: &JobId, diff: &[u8]) -> DependencyView {
    DependencyView {
        job_id: id.clone(),
        state: JobState::Done,
        diff_bytes: diff.len() as u64,
    }
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
        provider: None,
        model: None,
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
        provider: None,
        model: None,
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
        provider: None,
        model: None,
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
        provider: None,
        model: None,
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

// ---------------------------------------------------------------------------
// Job-level tests. Each one scripts `poll` to assign one job, lets the job run
// until the expected terminal state is recorded, then shuts the agent down.
// ---------------------------------------------------------------------------

/// A temp root with a work directory and a one-file git repository. The temp dir lives as long
/// as the fixture.
struct Fixture {
    _tmp: TempDir,
    work_dir: PathBuf,
    repo_dir: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);
    Fixture {
        _tmp: tmp,
        work_dir,
        repo_dir,
    }
}

/// A harness that runs `script` under `sh -c`.
fn sh(script: &'static str) -> impl Fn(&Path) -> CommandSpec + Send + Sync + 'static {
    move |_task_file| CommandSpec::new("sh").arg("-c").arg(script)
}

/// An agent whose heartbeat fires after one second, so control-loop cancels arrive quickly.
fn fast_agent(
    work_dir: &Path,
    controller: Arc<FakeController>,
    script: impl Fn(&Path) -> CommandSpec + Send + Sync + 'static,
) -> Agent {
    let mut agent = make_agent(work_dir, controller, script);
    agent.heartbeat_secs = 1;
    agent
}

/// A controller whose first poll assigns `job_id`. Later polls are empty unless the caller
/// pushes more replies first.
fn assigned_controller(job_id: &JobId, spec: JobSpec) -> Arc<FakeController> {
    let controller = Arc::new(FakeController::new());
    controller.push_poll(Ok(vec![AgentEvent::Assign {
        job_id: job_id.clone(),
        epoch: EPOCH,
        spec,
        provider: None,
        model: None,
    }]));
    controller
}

/// Ready once the job has completed and its workspace is gone.
fn job_finished(workspace: PathBuf) -> impl Fn(&[Call]) -> bool {
    move |calls| has_complete(calls) && !workspace.exists()
}

/// Ready once `trigger` has been recorded and the workspace is gone, for jobs that stop
/// without completing.
fn job_stopped(workspace: PathBuf, trigger: fn(&Call) -> bool) -> impl Fn(&[Call]) -> bool {
    move |calls| calls.iter().any(trigger) && !workspace.exists()
}

fn has_complete(calls: &[Call]) -> bool {
    calls.iter().any(|c| matches!(c, Call::Complete(_)))
}

/// Runs the agent until `done` holds over the recorded calls, then sends shutdown and waits for
/// the agent to stop. Returns every recorded call, polls included.
async fn run_job(
    controller: &Arc<FakeController>,
    agent: Agent,
    done: impl Fn(&[Call]) -> bool,
) -> Vec<Call> {
    let (shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let deadline = Instant::now() + JOB_TIMEOUT;
    while !controller.check(|calls| done(calls)) {
        assert!(
            Instant::now() < deadline,
            "job never reached the expected state; {} calls recorded",
            controller.check(|calls| calls.len())
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = shutdown_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("agent did not stop after shutdown")
        .expect("agent task panicked");
    assert!(result.is_ok(), "agent run failed: {result:?}");
    controller.take_calls()
}

/// The job's own calls, with the idle and heartbeat polls removed.
fn job_calls(calls: &[Call]) -> Vec<Call> {
    calls
        .iter()
        .filter(|c| !matches!(c, Call::Poll(_)))
        .cloned()
        .collect()
}

fn kind(call: &Call) -> &'static str {
    match call {
        Call::Poll(_) => "poll",
        Call::JobBase { .. } => "job_base",
        Call::Start { .. } => "start",
        Call::Renew { .. } => "renew",
        Call::AppendLog(_) => "append_log",
        Call::PutArtifact { .. } => "put_artifact",
        Call::Deps { .. } => "deps",
        Call::DepDiff { .. } => "dep_diff",
        Call::Complete(_) => "complete",
    }
}

fn completions(calls: &[Call]) -> Vec<Completion> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Complete(completion) => Some(completion.clone()),
            _ => None,
        })
        .collect()
}

fn appended(calls: &[Call]) -> Vec<(u64, String)> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::AppendLog(chunk) => Some((chunk.seq, chunk.data.clone())),
            _ => None,
        })
        .collect()
}

fn artifact(calls: &[Call], wanted: ArtifactName) -> Vec<u8> {
    calls
        .iter()
        .find_map(|c| match c {
            Call::PutArtifact { name, bytes, .. } if *name == wanted => Some(bytes.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {wanted:?} artifact uploaded"))
}

fn log_text(calls: &[Call]) -> String {
    appended(calls).into_iter().map(|(_, data)| data).collect()
}

#[tokio::test]
async fn job_happy_path_calls_in_order() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo working; echo added >> README.md; echo done"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let job = job_calls(&calls);
    let kinds: Vec<&str> = job.iter().map(kind).collect();
    assert_eq!(&kinds[..2], ["deps", "start"], "{kinds:?}");
    let n = kinds.len();
    assert!(n > 5, "{kinds:?}");
    assert_eq!(
        &kinds[n - 3..],
        ["put_artifact", "put_artifact", "complete"]
    );
    assert!(
        kinds[2..n - 3].iter().all(|k| *k == "append_log"),
        "{kinds:?}"
    );

    assert!(matches!(
        &job[1],
        Call::Start { job_id: started, epoch: EPOCH } if *started == job_id
    ));
    let names: Vec<ArtifactName> = job
        .iter()
        .filter_map(|c| match c {
            Call::PutArtifact { name, epoch, .. } => {
                assert_eq!(*epoch, EPOCH);
                Some(*name)
            }
            _ => None,
        })
        .collect();
    assert_eq!(names, [ArtifactName::DiffPatch, ArtifactName::ReportJson]);

    let diff = String::from_utf8(artifact(&calls, ArtifactName::DiffPatch)).unwrap();
    assert!(diff.contains("+added"), "{diff}");
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["outcome"], "done");

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Done);
    assert_eq!(done[0].exit_code, Some(0));
    assert_eq!(done[0].job_id, job_id);
    assert_eq!(done[0].epoch, EPOCH);
    assert!(log_text(&calls).contains("working"));
}

#[tokio::test]
async fn dependency_diffs_stack_in_after_order() {
    let f = fixture();
    let job_id = JobId::new();
    let dep1 = JobId::new();
    let dep2 = JobId::new();
    let first = make_diff(&f.repo_dir, "README.md", "test\n", "test\nfirst\n");
    let both = make_diff(&f.repo_dir, "README.md", "test\n", "test\nfirst\nsecond\n");

    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep1.clone(), dep2.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![
        dependency(&dep1, &first),
        dependency(&dep2, &both),
    ]));
    controller.push_dep_diff(Ok(first));
    controller.push_dep_diff(Ok(both));

    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^first$' README.md && grep -q '^second$' README.md"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let diff_calls: Vec<JobId> = calls
        .iter()
        .filter_map(|c| match c {
            Call::DepDiff { dep, .. } => Some(dep.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(diff_calls, [dep1.clone(), dep2.clone()]);
    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    let diff = String::from_utf8(artifact(&calls, ArtifactName::DiffPatch)).unwrap();
    assert!(diff.contains("+first"), "{diff}");
    assert!(diff.contains("+second"), "{diff}");
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(
        report["applied_dependencies"],
        json!([dep1.to_string(), dep2.to_string()])
    );
}

#[tokio::test]
async fn compatible_dependency_changes_to_one_file_merge_in_order() {
    let f = fixture();
    let job_id = JobId::new();
    let dep1 = JobId::new();
    let dep2 = JobId::new();
    let first = make_diff(&f.repo_dir, "README.md", "test\n", "first\ntest\n");
    let second = make_diff(&f.repo_dir, "README.md", "test\n", "test\nsecond\n");

    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep1.clone(), dep2.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![
        dependency(&dep1, &first),
        dependency(&dep2, &second),
    ]));
    controller.push_dep_diff(Ok(first));
    controller.push_dep_diff(Ok(second));

    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^first$' README.md && grep -q '^second$' README.md"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(
        report["applied_dependencies"],
        json!([dep1.to_string(), dep2.to_string()])
    );
}

#[tokio::test]
async fn conflicting_dependency_fails_before_the_harness() {
    let f = fixture();
    let job_id = JobId::new();
    let dep1 = JobId::new();
    let dep2 = JobId::new();
    let first = make_diff(&f.repo_dir, "README.md", "test\n", "one\n");
    let second = make_diff(&f.repo_dir, "README.md", "test\n", "two\n");

    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep1.clone(), dep2.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![
        dependency(&dep1, &first),
        dependency(&dep2, &second),
    ]));
    controller.push_dep_diff(Ok(first));
    controller.push_dep_diff(Ok(second));

    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo harness-ran"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert!(
        !calls.iter().any(|c| matches!(c, Call::Start { .. })),
        "a conflict must stop before the harness: {calls:?}"
    );
    assert!(!log_text(&calls).contains("harness-ran"));
    let done = completions(&calls);
    assert_eq!(
        done[0].outcome,
        Outcome::Failed(FailureClass::Conflict),
        "{done:?}"
    );
    let summary = done[0].summary.as_deref().unwrap_or("");
    assert!(summary.contains(dep2.as_str()), "{summary}");
    assert!(summary.contains("README.md"), "{summary}");
}

#[tokio::test]
async fn dependency_without_a_diff_is_skipped() {
    let f = fixture();
    let job_id = JobId::new();
    let dep = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![dependency(&dep, b"")]));

    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^test$' README.md && ! grep -q '^dep$' README.md"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert!(
        !calls.iter().any(|c| matches!(c, Call::DepDiff { .. })),
        "{calls:?}"
    );
    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(report["applied_dependencies"], json!([]));
}

#[tokio::test]
async fn no_stack_leaves_the_base_tree_alone() {
    let f = fixture();
    let job_id = JobId::new();
    let dep = JobId::new();
    let diff = make_diff(&f.repo_dir, "README.md", "test\n", "test\ndep\n");
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep.clone()];
    spec.stack_dependencies = false;
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![dependency(&dep, &diff)]));

    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^test$' README.md && ! grep -q '^dep$' README.md"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert!(
        !calls.iter().any(|c| matches!(c, Call::Deps { .. })),
        "{calls:?}"
    );
    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
}

#[tokio::test]
async fn job_log_offset_resumes_at_conflict_length() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    // "first\n" is stored, then "second\n" conflicts: the controller already holds ten bytes,
    // so only the four bytes past offset 6 are missing from the stored stream.
    controller.push_append_log(Ok(()));
    controller.push_append_log(Err(ApiError::Conflict { length: 10 }));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo first; sleep 0.5; echo second"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(
        appended(&calls),
        vec![
            (0, "first\n".to_owned()),
            (6, "second\n".to_owned()),
            (10, "nd\n".to_owned()),
        ]
    );
    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
}

#[tokio::test]
async fn job_failed_exit_completes_execution_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo failure; exit 1"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Execution));
    assert_eq!(done[0].exit_code, Some(1));
}

#[tokio::test]
async fn job_provider_marker_completes_provider_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo \"every key in this stream's chain is unavailable\"; exit 1"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Provider));
    assert_eq!(done[0].exit_code, Some(1));
}

#[tokio::test]
async fn job_provider_marker_then_silence_fails_as_provider() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let mut agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo \"every key in this stream's chain is unavailable\"; sleep 30"),
    );
    agent.provider_marker_grace = Duration::from_millis(300);
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Provider));
    assert_eq!(done[0].exit_code, None);
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(
        summary.contains("every key in this stream's chain is unavailable"),
        "{summary}"
    );
}

#[tokio::test]
async fn job_provider_marker_then_output_keeps_running() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let mut agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo \"every key in this stream's chain is unavailable\"; \
            sleep 0.15; echo more; sleep 0.15; echo more; sleep 0.15; echo more; exit 0"),
    );
    agent.provider_marker_grace = Duration::from_millis(300);
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Done);
    assert_eq!(done[0].exit_code, Some(0));
}

#[tokio::test]
async fn job_that_recovers_after_a_marker_is_not_killed_when_it_goes_quiet() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let mut agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo \"every key in this stream's chain is unavailable\"; \
            sleep 0.1; echo recovered; sleep 0.9; exit 0"),
    );
    agent.provider_marker_grace = Duration::from_millis(300);
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Done);
}

#[tokio::test]
async fn job_whose_provider_goes_unusable_in_reiki_fails_as_provider() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.provider = Some("zen".to_owned());
    let controller = assigned_controller(&job_id, spec);
    let mut agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo started; sleep 30"),
    );
    agent.reiki_watch_period = Duration::from_millis(100);
    let health = agent.clone_health_for_tests();
    let publisher = tokio::spawn(async move {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let mut guard = health.lock().unwrap();
            guard.seq += 1;
            guard.providers = vec![lu_proto::ProviderStatus {
                name: "zen".to_owned(),
                state: lu_proto::ProviderState::Cooling,
                soonest_secs: Some(90),
            }];
        }
    });
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;
    publisher.abort();

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Provider));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(
        summary.contains("zen") && summary.contains("90s"),
        "{summary}"
    );
    assert_eq!(
        done[0].retry_after_secs,
        Some(150),
        "90s from Reiki plus the buffer"
    );
}

#[tokio::test]
async fn job_survives_a_provider_that_recovers_between_readings() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.provider = Some("zen".to_owned());
    let controller = assigned_controller(&job_id, spec);
    let mut agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo started; sleep 1.4; exit 0"),
    );
    agent.reiki_watch_period = Duration::from_millis(40);
    let health = agent.clone_health_for_tests();
    let publisher = tokio::spawn(async move {
        for i in 0..12 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut guard = health.lock().unwrap();
            guard.seq += 1;
            guard.providers = vec![lu_proto::ProviderStatus {
                name: "zen".to_owned(),
                state: if i % 2 == 0 {
                    lu_proto::ProviderState::Cooling
                } else {
                    lu_proto::ProviderState::Ok
                },
                soonest_secs: None,
            }];
        }
    });
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;
    publisher.abort();

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Done);
}

#[tokio::test]
async fn job_uses_the_assigned_model() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.provider = Some("p1".to_owned());
    spec.model = Some("primary-model".to_owned());
    let controller = Arc::new(FakeController::new());
    controller.push_poll(Ok(vec![AgentEvent::Assign {
        job_id: job_id.clone(),
        epoch: EPOCH,
        spec,
        provider: Some("p2".to_owned()),
        model: Some("fallback-model".to_owned()),
    }]));
    controller.push_start(Ok(()));
    controller.push_renew(Ok(false));
    controller.push_complete(Ok(()));
    let seen = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&seen);
    let agent = make_agent(&f.work_dir, controller.clone(), success_harness).with_command_builder(
        move |task_file, model, _server, _message| {
            *recorder.lock().unwrap() = model.map(str::to_owned);
            Ok(success_harness(task_file))
        },
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(*seen.lock().unwrap(), Some("fallback-model".to_owned()));
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(report["model"], "fallback-model");
}

#[tokio::test]
async fn job_unsupported_harness_completes_execution_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.harness = "pi".to_owned();
    let controller = assigned_controller(&job_id, spec);
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo should not run"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Execution));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(summary.contains("unsupported harness"), "{summary}");
    assert!(appended(&calls).is_empty(), "the harness must not run");
}

#[tokio::test]
async fn job_whose_harness_cannot_start_completes_node_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(&f.work_dir, controller.clone(), |_task_file| {
        CommandSpec::new("no-such-harness-binary-for-lu-tests")
    });
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Node));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(
        summary.starts_with("harness could not be started"),
        "{summary}"
    );
}

#[tokio::test]
async fn job_whose_ref_does_not_exist_completes_execution_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.git_ref = "no-such-ref".to_owned();
    let controller = assigned_controller(&job_id, spec);
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo should not run"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Execution));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(summary.starts_with("staging failed"), "{summary}");
    assert!(
        !calls.iter().any(|c| matches!(c, Call::Start { .. })),
        "a job that cannot be staged must not be started"
    );
}

#[tokio::test]
async fn job_whose_stage_directory_already_exists_completes_node_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    // A leftover directory from an attempt the controller re-leased.
    std::fs::create_dir(f.work_dir.join(job_id.as_str())).unwrap();
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo should not run"));
    let calls = run_job(&controller, agent, has_complete).await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Node));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(summary.starts_with("staging failed"), "{summary}");
    assert!(
        !calls.iter().any(|c| matches!(c, Call::Start { .. })),
        "a job that cannot be staged must not be started"
    );
}

#[tokio::test]
async fn job_missing_repository_completes_execution_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let missing = f.work_dir.parent().unwrap().join("no-such-repo");
    let controller = assigned_controller(&job_id, test_spec(&missing.to_string_lossy()));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo should not run"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Execution));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(summary.starts_with("staging failed"), "{summary}");
    assert!(
        !calls.iter().any(|c| matches!(c, Call::Start { .. })),
        "a job that cannot be staged must not be started"
    );
}

#[tokio::test]
async fn job_child_env_is_allowlisted() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("env"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let log = log_text(&calls);
    let names: Vec<&str> = log
        .lines()
        .filter_map(|line| line.split_once('=').map(|(key, _)| key))
        .collect();
    assert!(names.contains(&"PATH"), "env was not dumped: {log}");
    let allowed = ["PATH", "HOME", "LANG", "PWD", "SHLVL", "_", "OLDPWD"];
    for name in &names {
        assert!(allowed.contains(name), "unexpected variable {name}: {log}");
    }
    let lower = log.to_ascii_lowercase();
    assert!(!lower.contains("token"), "{log}");
    assert!(!lower.contains("http"), "{log}");
}

#[tokio::test]
async fn job_harness_gets_the_opencode_password_only_when_one_is_configured() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let mut agent = fast_agent(&f.work_dir, controller.clone(), sh("env"));
    agent.opencode_password = Some(Arc::from("test-password-value"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;
    let log = log_text(&calls);
    assert!(
        log.lines()
            .any(|line| line == "OPENCODE_SERVER_PASSWORD=test-password-value"),
        "the harness did not get the password: {log}"
    );
}

#[tokio::test]
async fn heartbeats_carry_the_providers_the_local_reiki_reports() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf).await;
                let seen = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                let body = if seen.contains("authorization: bearer probe-token") {
                    r#"{"instance":"node-a","upstreams":[{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0},{"name":"openrouter","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":60}]}"#
                } else {
                    "{}"
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    let f = fixture();
    let controller = Arc::new(FakeController::new());
    for _ in 0..200 {
        controller.push_poll(Ok(Vec::new()));
    }
    let mut agent = make_agent(&f.work_dir, controller.clone(), sh("true"));
    agent.reiki_probe = Some((
        format!("http://127.0.0.1:{port}/v1/health"),
        Arc::from("probe-token"),
    ));
    let (stop, handle) = run_agent_with_shutdown(agent);
    let deadline = Instant::now() + Duration::from_secs(5);
    let carried = loop {
        let found = controller.check(|calls| {
            calls.iter().find_map(|call| match call {
                Call::Poll(request) if !request.providers.is_empty() => Some(request.clone()),
                _ => None,
            })
        });
        if let Some(request) = found {
            break request;
        }
        assert!(Instant::now() < deadline, "no heartbeat carried providers");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let names: Vec<&str> = carried.providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["zen", "openrouter"]);
    assert_eq!(carried.providers[1].soonest_secs, Some(60));
    assert!(carried.reiki.is_some(), "the document is forwarded too");
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}

#[tokio::test]
async fn job_poll_unauthorized_ends_agent() {
    let f = fixture();
    let controller = Arc::new(FakeController::new());
    controller.push_poll(Err(ApiError::Unauthorized));
    let agent = make_agent(&f.work_dir, controller.clone(), sh("true"));
    let (_shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("agent did not end")
        .expect("agent task panicked");
    assert!(
        matches!(result, Err(AgentError::Unauthorized)),
        "{result:?}"
    );
}

#[tokio::test]
async fn job_transient_poll_errors_back_off_and_recover() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = Arc::new(FakeController::new());
    controller.push_poll(Err(ApiError::Network {
        message: "connection reset".to_owned(),
    }));
    controller.push_poll(Err(ApiError::Network {
        message: "connection reset".to_owned(),
    }));
    controller.push_poll(Ok(vec![AgentEvent::Assign {
        job_id: job_id.clone(),
        epoch: EPOCH,
        spec: test_spec(&f.repo_dir.to_string_lossy()),
        provider: None,
        model: None,
    }]));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo recovered"));
    // Backoff uses tokio timers on the real clock (the test-util feature is not enabled), so
    // the first two delays are at least one and two seconds.
    let started = Instant::now();
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "retries did not back off: {:?}",
        started.elapsed()
    );
    let polls = calls.iter().filter(|c| matches!(c, Call::Poll(_))).count();
    assert!(polls >= 3, "expected at least three polls, saw {polls}");
    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
}

#[tokio::test]
async fn job_stale_start_completes_nothing() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    controller.push_start(Err(ApiError::StaleEpoch));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo should not run"));
    let calls = run_job(
        &controller,
        agent,
        job_stopped(f.work_dir.join(job_id.as_str()), |c| {
            matches!(c, Call::Start { .. })
        }),
    )
    .await;

    assert!(completions(&calls).is_empty(), "{calls:?}");
    assert!(appended(&calls).is_empty());
}

#[tokio::test]
async fn job_stale_log_append_stops_without_completing() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    controller.push_append_log(Err(ApiError::StaleEpoch));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo hi; sleep 30"));
    let calls = run_job(
        &controller,
        agent,
        job_stopped(f.work_dir.join(job_id.as_str()), |c| {
            matches!(c, Call::AppendLog(_))
        }),
    )
    .await;

    assert!(completions(&calls).is_empty(), "{calls:?}");
    assert!(
        !calls.iter().any(|c| matches!(c, Call::PutArtifact { .. })),
        "a stale job must not upload artifacts"
    );
}

#[tokio::test]
async fn job_renew_true_stops_harness_and_reports_cancelled() {
    // The first renew fires after the 20 second renew period, so this test is slow.
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    controller.push_renew(Ok(true));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo started; sleep 60"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_stopped(f.work_dir.join(job_id.as_str()), |c| {
            matches!(c, Call::Renew { .. })
        }),
    )
    .await;

    assert!(
        calls.iter().any(|c| matches!(c, Call::Renew { .. })),
        "the lease was never renewed"
    );
    assert!(
        completions(&calls)
            .iter()
            .all(|c| c.outcome == Outcome::Cancelled),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| matches!(c, Call::PutArtifact { .. })),
        "a cancelled job must not upload artifacts"
    );
}

#[tokio::test]
async fn job_renew_stale_stops_without_completing() {
    // The first renew fires after the 20 second renew period, so this test is slow.
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    controller.push_renew(Err(ApiError::StaleEpoch));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo started; sleep 60"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_stopped(f.work_dir.join(job_id.as_str()), |c| {
            matches!(c, Call::Renew { .. })
        }),
    )
    .await;

    assert!(completions(&calls).is_empty(), "{calls:?}");
}

#[tokio::test]
async fn job_workspace_removed_after_success() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo ok"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    assert!(!f.work_dir.join(job_id.as_str()).exists());
}

#[tokio::test]
async fn job_workspace_removed_after_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo bad; exit 3"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(
        completions(&calls)[0].outcome,
        Outcome::Failed(FailureClass::Execution)
    );
    assert!(!f.work_dir.join(job_id.as_str()).exists());
}

#[tokio::test]
async fn job_workspace_removed_after_cancel() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    // The first heartbeat poll, one second in, carries the cancel.
    controller.push_poll(Ok(vec![AgentEvent::Cancel {
        job_id: job_id.clone(),
        epoch: EPOCH,
    }]));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo started; sleep 30"),
    );
    let workspace = f.work_dir.join(job_id.as_str());
    let calls = run_job(&controller, agent, {
        let workspace = workspace.clone();
        move |calls| calls.iter().any(|c| matches!(c, Call::Start { .. })) && !workspace.exists()
    })
    .await;

    assert!(
        completions(&calls)
            .iter()
            .all(|c| c.outcome == Outcome::Cancelled),
        "{calls:?}"
    );
    assert!(!workspace.exists());
}

#[tokio::test]
async fn job_output_over_cap_is_bounded_and_completes() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    // Sixty megabytes against a fifty megabyte cap.
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("yes 'a line of harness output' | head -c 60000000"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let uploaded: u64 = appended(&calls)
        .iter()
        .map(|(_, data)| data.len() as u64)
        .sum();
    assert!(uploaded > 0, "no log reached the controller");
    assert!(
        uploaded <= MAX_LOG_BYTES,
        "uploaded {uploaded} bytes, cap is {MAX_LOG_BYTES}"
    );
    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Execution));
}

#[tokio::test]
async fn job_shutdown_while_running_reports_node_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo started; sleep 10"),
    );
    let calls = run_job(&controller, agent, |calls| {
        log_text(calls).contains("started")
    })
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Node));
}

#[tokio::test]
async fn auto_base_source_falls_back_to_clone_when_the_controller_has_no_base() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let mut spec = test_spec(&repo_dir.to_string_lossy());
    spec.base_commit = Some("a".repeat(40));
    let controller = controller_with_assign(job_id.clone(), EPOCH, spec);
    controller.push_base(Err(ApiError::NotFound));

    let agent = make_agent(&work_dir, controller.clone(), success_harness);
    let calls = run_job(
        &controller,
        agent,
        job_finished(work_dir.join(job_id.as_str())),
    )
    .await;
    assert!(calls.iter().any(|c| matches!(c, Call::JobBase { .. })));
    assert!(has_complete(&calls));
}

#[tokio::test]
async fn controller_base_source_fails_the_node_when_the_controller_has_no_base() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let mut spec = test_spec(&repo_dir.to_string_lossy());
    spec.base_commit = Some("a".repeat(40));
    let controller = controller_with_assign(job_id.clone(), EPOCH, spec);
    controller.push_base(Err(ApiError::NotFound));

    let mut agent = make_agent(&work_dir, controller.clone(), success_harness);
    agent.base_source = BaseSource::Controller;
    let (_shutdown_tx, handle) = run_agent_with_shutdown(agent);
    let result = tokio::time::timeout(JOB_TIMEOUT, handle)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, Err(AgentError::Controller(_))),
        "a controller-mode miss must be a node failure: {result:?}"
    );
    let calls = controller.take_calls();
    assert!(calls.iter().any(|c| matches!(c, Call::JobBase { .. })));
    assert!(!has_complete(&calls));
}

#[tokio::test]
async fn clone_base_source_never_asks_the_controller() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);

    let job_id = JobId::new();
    let mut spec = test_spec(&repo_dir.to_string_lossy());
    spec.base_commit = Some("a".repeat(40));
    let controller = controller_with_assign(job_id.clone(), EPOCH, spec);

    let mut agent = make_agent(&work_dir, controller.clone(), success_harness);
    agent.base_source = BaseSource::Clone;
    let calls = run_job(
        &controller,
        agent,
        job_finished(work_dir.join(job_id.as_str())),
    )
    .await;
    assert!(!calls.iter().any(|c| matches!(c, Call::JobBase { .. })));
    assert!(has_complete(&calls));
}
