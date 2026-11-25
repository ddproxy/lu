//! Unit tests for the agent loop. Uses a scripted `Controller` and a `sh -c` harness.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lu_client::{ApiError, Controller, DependencyView, PollRequest};
use lu_config::{BaseSource, StackConfig};
use lu_proto::{
    AgentEvent, ArtifactName, Completion, ExpectMode, ExpectSpec, FailureClass, HarnessState,
    HostStats, JobId, JobSpec, JobState, LogChunk, NodeName, OnConflict, Outcome, StuckPolicy,
    WorkspacePolicy, WorkspaceState, HARNESS_OK_FALSE, MAX_LOG_BYTES,
};
use lu_staging::{ExportedDiff, StackPolicy, Staging};
use lu_supervise::CommandSpec;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::{
    dependency_conflict_summary, disk_free_pct, evaluate_expect, housekeeping,
    workspaces::FsDeleter, workspaces::RealClock, workspaces::Registry, workspaces::Settings,
    Agent, AgentError, ConflictContext,
};

const TEST_HEARTBEAT_SECS: u64 = 5;
const IDLE_POLL: Duration = Duration::from_millis(10);
const JOB_TIMEOUT: Duration = Duration::from_secs(30);
const EPOCH: u64 = 1;

type BaseReply = Result<Option<(String, Vec<u8>)>, ApiError>;

/// A recorded call to the fake controller.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Call {
    Poll(Box<PollRequest>),
    JobBase {
        job_id: JobId,
        epoch: u64,
    },
    StagedCommit {
        job_id: JobId,
        epoch: u64,
        commit: String,
    },
    MergeBase {
        job_id: JobId,
        epoch: u64,
        commits: Vec<String>,
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
    Scope(Box<lu_proto::ScopeRequest>),
}

/// A fake controller that records calls and returns scripted replies.
pub(crate) struct FakeController {
    calls: Mutex<Vec<Call>>,
    poll_replies: Mutex<VecDeque<Result<Vec<AgentEvent>, ApiError>>>,
    base_replies: Mutex<VecDeque<BaseReply>>,
    staged_commit_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    merge_base_replies: Mutex<VecDeque<Result<String, ApiError>>>,
    start_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    renew_replies: Mutex<VecDeque<Result<bool, ApiError>>>,
    append_log_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    put_artifact_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    deps_replies: Mutex<VecDeque<Result<Vec<DependencyView>, ApiError>>>,
    dep_diff_replies: Mutex<VecDeque<Result<Vec<u8>, ApiError>>>,
    complete_replies: Mutex<VecDeque<Result<(), ApiError>>>,
    scope_replies: Mutex<VecDeque<Result<lu_proto::ScopeResponse, ApiError>>>,
}

impl FakeController {
    pub(crate) fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            poll_replies: Mutex::new(VecDeque::new()),
            base_replies: Mutex::new(VecDeque::new()),
            staged_commit_replies: Mutex::new(VecDeque::new()),
            merge_base_replies: Mutex::new(VecDeque::new()),
            start_replies: Mutex::new(VecDeque::new()),
            renew_replies: Mutex::new(VecDeque::new()),
            append_log_replies: Mutex::new(VecDeque::new()),
            put_artifact_replies: Mutex::new(VecDeque::new()),
            deps_replies: Mutex::new(VecDeque::new()),
            dep_diff_replies: Mutex::new(VecDeque::new()),
            complete_replies: Mutex::new(VecDeque::new()),
            scope_replies: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    pub(crate) fn take_calls(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    /// Runs `f` over the recorded calls without taking them.
    pub(crate) fn check<R>(&self, f: impl FnOnce(&[Call]) -> R) -> R {
        f(&self.calls.lock().unwrap())
    }

    fn push_poll(&self, reply: Result<Vec<AgentEvent>, ApiError>) {
        self.poll_replies.lock().unwrap().push_back(reply);
    }

    fn push_base(&self, reply: Result<Option<(String, Vec<u8>)>, ApiError>) {
        self.base_replies.lock().unwrap().push_back(reply);
    }

    fn push_staged_commit(&self, reply: Result<(), ApiError>) {
        self.staged_commit_replies.lock().unwrap().push_back(reply);
    }

    fn push_merge_base(&self, reply: Result<String, ApiError>) {
        self.merge_base_replies.lock().unwrap().push_back(reply);
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

    pub(crate) fn push_scope(&self, reply: Result<lu_proto::ScopeResponse, ApiError>) {
        self.scope_replies.lock().unwrap().push_back(reply);
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
        self.record(Call::Poll(Box::new(request)));
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

    fn set_staged_commit<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
        commit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        self.record(Call::StagedCommit {
            job_id: id.clone(),
            epoch,
            commit: commit.to_owned(),
        });
        Box::pin(async move {
            self.staged_commit_replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(()))
        })
    }

    fn merge_base<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
        commits: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<String, ApiError>> + Send + 'a>> {
        self.record(Call::MergeBase {
            job_id: id.clone(),
            epoch,
            commits: commits.to_vec(),
        });
        Box::pin(async move {
            self.merge_base_replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(commits[0].clone()))
        })
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

    fn scope<'a>(
        &'a self,
        request: &'a lu_proto::ScopeRequest,
    ) -> Pin<Box<dyn Future<Output = Result<lu_proto::ScopeResponse, ApiError>> + Send + 'a>> {
        self.record(Call::Scope(Box::new(request.clone())));
        Box::pin(async move {
            self.scope_replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(lu_proto::ScopeResponse::new(
                        lu_proto::ScopeResult::JobContext {
                            job_id: JobId::new(),
                            state: "running".to_owned(),
                            spec: Box::new(test_spec("https://example.invalid/repo.git")),
                            dependencies: Vec::new(),
                        },
                    ))
                })
        })
    }
}

/// Builds a minimal valid JobSpec for testing.
fn test_spec(repo: &str) -> JobSpec {
    JobSpec {
        source: lu_proto::SourceSpec {
            repo: repo.to_owned(),
            git_ref: "main".to_owned(),
            base: None,
        },
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
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
        collect: lu_proto::CollectMode::Apply,
        include_failed: false,
        on_conflict: Default::default(),
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        on_stuck: None,
        stuck_after_secs: None,
        priority: lu_proto::DEFAULT_PRIORITY,
        group: None,
        allow_spawn: false,
        spawn_inherit: false,
        spawn_repos: Vec::new(),
        workspace: None,
        cache_dirs: Vec::new(),
        context: None,
        origin: None,
        origin_ref: None,
        untrusted_context: false,
        draft: false,
        publish: None,
        fallbacks: Vec::new(),
        expect: None,
        schedule: None,
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
    let workspaces = Arc::new(
        Registry::new(
            Settings::new(work_dir.to_path_buf()),
            Arc::new(RealClock),
            Arc::new(FsDeleter),
            Arc::new(housekeeping::disk_space as fn(&Path) -> Option<housekeeping::DiskSpace>),
        )
        .unwrap(),
    );
    Agent {
        node,
        controller,
        staging,
        base_source: BaseSource::Auto,
        stack: None,
        work_dir: work_dir.to_path_buf(),
        heartbeat_secs: TEST_HEARTBEAT_SECS,
        default_on_stuck: lu_proto::StuckPolicy::None,
        default_stuck_after_secs: 1_800,
        stuck_detect_secs: super::proc::STUCK_AFTER_SECS,
        stuck_poll_period: super::STUCK_POLL_PERIOD,
        stuck_after_override: None,
        stuck_clock: Arc::new(super::proc::RealClock),
        labels: Arc::new(super::labels::LabelSource::new(Vec::new(), Vec::new())),
        probes: Arc::new(super::probes::Probes::new(Vec::new())),
        taints: Arc::new(Vec::new()),
        harness_ok: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        provider_marker_grace: super::PROVIDER_MARKER_GRACE,
        provider_health_watch_period: super::PROVIDER_HEALTH_WATCH_PERIOD,
        opencode_server: None,
        workspace_gid: None,
        harness_owner: None,
        opencode_password: None,
        workspaces,
        provider_health_probe: None,
        gateway_fault_markers: Arc::new(Vec::new()),
        health: Default::default(),
        provider_models: Arc::new(Vec::new()),
        harness: "opencode".to_owned(),
        harness_wrapper: Vec::new(),
        allow_shared_uid_scope: true,
        builder: Arc::new(move |task_file, _model, _server, _message| {
            Ok(harness_script(task_file))
        }),
        housekeeper: None,
        gc_counters: Arc::new(housekeeping::GcCounters::default()),
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
fn head_commit(repo: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

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

fn dependency(id: &JobId, diff: &[u8], staged_commit: &str) -> DependencyView {
    DependencyView {
        job_id: id.clone(),
        state: JobState::Done,
        diff_bytes: diff.len() as u64,
        idempotency_key: None,
        node: None,
        assigned_provider: None,
        assigned_model: None,
        started_at: None,
        finished_at: None,
        failure_class: None,
        summary: None,
        staged_commit: Some(staged_commit.to_owned()),
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
    /// HEAD at the time the fixture was made; dependencies inherit this base.
    base_commit: String,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);
    let base_commit = head_commit(&repo_dir);
    Fixture {
        _tmp: tmp,
        work_dir,
        repo_dir,
        base_commit,
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
        Call::StagedCommit { .. } => "staged_commit",
        Call::MergeBase { .. } => "merge_base",
        Call::Start { .. } => "start",
        Call::Renew { .. } => "renew",
        Call::AppendLog(_) => "append_log",
        Call::PutArtifact { .. } => "put_artifact",
        Call::Deps { .. } => "deps",
        Call::DepDiff { .. } => "dep_diff",
        Call::Complete(_) => "complete",
        Call::Scope(_) => "scope",
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
    assert_eq!(&kinds[..3], ["staged_commit", "deps", "start"], "{kinds:?}");
    let n = kinds.len();
    assert!(n > 6, "{kinds:?}");
    assert_eq!(
        &kinds[n - 3..],
        ["put_artifact", "put_artifact", "complete"]
    );
    assert!(
        kinds[3..n - 3].iter().all(|k| *k == "append_log"),
        "{kinds:?}"
    );

    assert!(matches!(
        &job[2],
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

fn exported(bytes: &str, paths: &[&str]) -> ExportedDiff {
    ExportedDiff {
        bytes: bytes.as_bytes().to_vec(),
        changed_paths: paths.iter().map(|p| (*p).to_owned()).collect(),
    }
}

#[test]
fn evaluate_expect_checks_each_kind_of_expectation() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("REVIEW.md"), b"review\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/main.rs"), b"fn main() {}\n").unwrap();
    let expect = ExpectSpec {
        files: vec!["REVIEW.md".to_owned(), "MISSING.md".to_owned()],
        changed: vec!["src/**".to_owned(), "tests/**".to_owned()],
        forbid_changed: vec!["Cargo.lock".to_owned()],
        min_diff_bytes: Some(12),
        max_diff_bytes: Some(64),
        mode: ExpectMode::Suspect,
    };
    let diff = exported("patch bytes", &["src/main.rs", "Cargo.lock"]);
    let (failures, outcome, _, suspect) =
        evaluate_expect(&expect, Some(&diff), tmp.path(), "harness summary");
    assert_eq!(outcome, Outcome::Done);
    assert!(suspect);
    assert!(failures.iter().any(|f| f.contains("MISSING.md")));
    assert!(failures.iter().any(|f| f.contains("tests/**")));
    assert!(failures.iter().any(|f| f.contains("Cargo.lock")));
    assert!(failures
        .iter()
        .any(|f| f.contains("below minimum") || f.contains("above maximum")));
    assert_eq!(failures.len(), 4, "{failures:?}");
}

#[test]
fn evaluate_expect_accepts_met_expectations_and_rejects_size_bounds() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("REVIEW.md"), b"review\n").unwrap();
    let expect = ExpectSpec {
        files: vec!["REVIEW.md".to_owned()],
        changed: vec!["**/*.md".to_owned()],
        forbid_changed: vec!["Cargo.lock".to_owned()],
        min_diff_bytes: Some(1),
        max_diff_bytes: Some(64),
        mode: ExpectMode::Suspect,
    };
    let diff = exported("patch", &["REVIEW.md", ".hidden.md"]);
    let (failures, outcome, summary, suspect) =
        evaluate_expect(&expect, Some(&diff), tmp.path(), "harness summary");
    assert!(failures.is_empty());
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(summary, "harness summary");
    assert!(!suspect);

    let too_small = ExpectSpec {
        min_diff_bytes: Some(4096),
        ..expect.clone()
    };
    let (failures, _, _, _) =
        evaluate_expect(&too_small, Some(&diff), tmp.path(), "harness summary");
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("below minimum"));

    let too_large = ExpectSpec {
        max_diff_bytes: Some(1),
        ..expect
    };
    let (failures, _, _, _) =
        evaluate_expect(&too_large, Some(&diff), tmp.path(), "harness summary");
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("above maximum"));
}

#[tokio::test]
async fn suspect_expectations_keep_the_job_done_and_record_report_failures() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.expect = Some(ExpectSpec {
        files: vec!["MISSING.md".to_owned()],
        mode: ExpectMode::Suspect,
        ..ExpectSpec::default()
    });
    let controller = assigned_controller(&job_id, spec);
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo done"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    let failures = report["expect_failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].as_str().unwrap().contains("MISSING.md"));
    let done = completions(&calls);
    assert_eq!(done[0].outcome, Outcome::Done);
    assert!(done[0].expect_failed);
}

#[tokio::test]
async fn fail_mode_expectations_fail_the_run_with_an_execution_class() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.expect = Some(ExpectSpec {
        changed: vec!["src/**".to_owned()],
        mode: ExpectMode::Fail,
        ..ExpectSpec::default()
    });
    let controller = assigned_controller(&job_id, spec);
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo done"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(
        report["outcome"],
        serde_json::json!({ "failed": "execution" })
    );
    let done = completions(&calls);
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Execution));
    assert!(!done[0].expect_failed);
    assert!(done[0]
        .summary
        .as_deref()
        .unwrap()
        .contains("expect failed"));
}

#[tokio::test]
async fn harness_failure_keeps_its_own_class_with_expectations_declared() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.expect = Some(ExpectSpec {
        changed: vec!["src/**".to_owned()],
        mode: ExpectMode::Fail,
        ..ExpectSpec::default()
    });
    let controller = assigned_controller(&job_id, spec);
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo broken; exit 1"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert!(report.get("expect_failures").is_none());
    let done = completions(&calls);
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Execution));
    assert!(!done[0].expect_failed);
    assert!(!done[0]
        .summary
        .as_deref()
        .unwrap()
        .contains("expect failed"));
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
        dependency(&dep1, &first, &f.base_commit),
        dependency(&dep2, &both, &f.base_commit),
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
        dependency(&dep1, &first, &f.base_commit),
        dependency(&dep2, &second, &f.base_commit),
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
        dependency(&dep1, &first, &f.base_commit),
        dependency(&dep2, &second, &f.base_commit),
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
    controller.push_deps(Ok(vec![dependency(&dep, b"", &f.base_commit)]));

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

/// Three inputs from one base: the second conflicts with the first, the third is independent.
fn three_conflicting_inputs(
    f: &mut Fixture,
) -> (JobId, JobId, JobId, JobId, Vec<u8>, Vec<u8>, Vec<u8>) {
    let job_id = JobId::new();
    let dep1 = JobId::new();
    let dep2 = JobId::new();
    let dep3 = JobId::new();
    std::fs::write(f.repo_dir.join("notes.md"), "base\n").unwrap();
    git(&f.repo_dir, &["add", "notes.md"]);
    git(&f.repo_dir, &["commit", "-q", "-m", "notes"]);
    f.base_commit = head_commit(&f.repo_dir);
    let first = make_diff(&f.repo_dir, "README.md", "test\n", "one\n");
    let second = make_diff(&f.repo_dir, "README.md", "test\n", "two\n");
    let third = make_diff(&f.repo_dir, "notes.md", "base\n", "third\n");
    (job_id, dep1, dep2, dep3, first, second, third)
}

fn conflict_controller(
    job_id: &JobId,
    repo: &str,
    deps: &[DependencyView],
    diffs: &[Vec<u8>],
    mode: OnConflict,
) -> Arc<FakeController> {
    let mut spec = test_spec(repo);
    spec.after = deps.iter().map(|dep| dep.job_id.clone()).collect();
    spec.on_conflict = mode;
    let controller = assigned_controller(job_id, spec);
    controller.push_deps(Ok(deps.to_vec()));
    for diff in diffs {
        controller.push_dep_diff(Ok(diff.clone()));
    }
    controller
}

#[tokio::test]
async fn fail_policy_names_the_conflicting_input_and_hunk() {
    let mut f = fixture();
    let (job_id, dep1, dep2, dep3, first, second, third) = three_conflicting_inputs(&mut f);
    let deps = vec![
        dependency(&dep1, &first, &f.base_commit),
        dependency(&dep2, &second, &f.base_commit),
        dependency(&dep3, &third, &f.base_commit),
    ];
    let controller = conflict_controller(
        &job_id,
        &f.repo_dir.to_string_lossy(),
        &deps,
        &[first, second, third],
        OnConflict::Fail,
    );
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
    let done = completions(&calls);
    assert_eq!(
        done[0].outcome,
        Outcome::Failed(FailureClass::Conflict),
        "{done:?}"
    );
    let summary = done[0].summary.as_deref().unwrap_or("");
    assert!(summary.contains(dep2.as_str()), "{summary}");
    assert!(summary.contains("README.md"), "{summary}");
    assert!(summary.contains("hunk at line"), "{summary}");
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(report["applied_dependencies"], json!([dep1.to_string()]));
}

#[tokio::test]
async fn skip_policy_omits_the_bad_patch_and_applies_later_inputs() {
    let mut f = fixture();
    let (job_id, dep1, dep2, dep3, first, second, third) = three_conflicting_inputs(&mut f);
    let deps = vec![
        dependency(&dep1, &first, &f.base_commit),
        dependency(&dep2, &second, &f.base_commit),
        dependency(&dep3, &third, &f.base_commit),
    ];
    let controller = conflict_controller(
        &job_id,
        &f.repo_dir.to_string_lossy(),
        &deps,
        &[first, second, third],
        OnConflict::Skip,
    );
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^one$' README.md && ! grep -q '^two$' README.md && grep -q '^third$' notes.md"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    let diff = String::from_utf8(artifact(&calls, ArtifactName::DiffPatch)).unwrap();
    assert!(diff.contains("+one"), "{diff}");
    assert!(!diff.contains("+two"), "{diff}");
    assert!(diff.contains("+third"), "{diff}");
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(
        report["applied_dependencies"],
        json!([dep1.to_string(), dep3.to_string()])
    );
}

#[tokio::test]
async fn markers_policy_keeps_both_sides_and_exports_them() {
    let mut f = fixture();
    let (job_id, dep1, dep2, dep3, first, second, third) = three_conflicting_inputs(&mut f);
    let deps = vec![
        dependency(&dep1, &first, &f.base_commit),
        dependency(&dep2, &second, &f.base_commit),
        dependency(&dep3, &third, &f.base_commit),
    ];
    let controller = conflict_controller(
        &job_id,
        &f.repo_dir.to_string_lossy(),
        &deps,
        &[first, second, third],
        OnConflict::Markers,
    );
    let script = format!(
        "grep -q '^<<<<<<< HEAD$' README.md && grep -q '^one$' README.md && grep -q '^two$' README.md && grep -q '^>>>>>>> {}$' README.md && grep -q '^third$' notes.md",
        dep2
    );
    let agent = fast_agent(&f.work_dir, controller.clone(), move |_| {
        CommandSpec::new("sh").arg("-c").arg(script.as_str())
    });
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    let diff = String::from_utf8(artifact(&calls, ArtifactName::DiffPatch)).unwrap();
    assert!(diff.contains("+<<<<<<< HEAD"), "{diff}");
    assert!(diff.contains("+one"), "{diff}");
    assert!(diff.contains("+two"), "{diff}");
    assert!(diff.contains(&format!("+>>>>>>> {dep2}")), "{diff}");
    assert!(diff.contains("+third"), "{diff}");
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(
        report["applied_dependencies"],
        json!([dep1.to_string(), dep2.to_string(), dep3.to_string()])
    );
}

#[tokio::test]
async fn collect_none_leaves_the_base_tree_alone() {
    let f = fixture();
    let job_id = JobId::new();
    let dep = JobId::new();
    let diff = make_diff(&f.repo_dir, "README.md", "test\n", "test\ndep\n");
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep.clone()];
    spec.collect = lu_proto::CollectMode::None;
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![dependency(&dep, &diff, &f.base_commit)]));

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

    // The dependency view is still needed to resolve the inherited base; only the diff is
    // skipped when collection is off.
    assert_eq!(
        calls
            .iter()
            .filter(|c| matches!(c, Call::Deps { .. }))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| matches!(c, Call::DepDiff { .. })),
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

/// Heartbeats after a crash advertise the broken harness as a label and a taint.
fn reports_broken_harness(poll: &PollRequest) -> bool {
    poll.labels.iter().any(|label| label == HARNESS_OK_FALSE)
        && poll.taints.iter().any(|taint| taint == HARNESS_OK_FALSE)
}

fn polls_after_completion(calls: &[Call]) -> Vec<&PollRequest> {
    let Some(index) = calls
        .iter()
        .position(|call| matches!(call, Call::Complete(_)))
    else {
        return Vec::new();
    };
    calls[index + 1..]
        .iter()
        .filter_map(|call| match call {
            Call::Poll(request) => Some(request.as_ref()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_harness_crash_at_launch_is_a_retryable_node_failure() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo 'Bun v1.1.0linux x86_64'; echo 'abort() called'; exit 1"),
    );
    let harness_ok = Arc::clone(&agent.harness_ok);
    let calls = run_job(&controller, agent, |calls| {
        has_complete(calls)
            && polls_after_completion(calls)
                .iter()
                .any(|poll| reports_broken_harness(poll))
    })
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Node));
    assert_eq!(done[0].exit_code, Some(1));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(summary.contains("abort() called"), "{summary}");
    assert!(!harness_ok.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn a_successful_harness_run_clears_the_broken_harness_label() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo success; exit 0"));
    agent
        .harness_ok
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let harness_ok = Arc::clone(&agent.harness_ok);
    let calls = run_job(&controller, agent, |calls| {
        has_complete(calls)
            && polls_after_completion(calls)
                .iter()
                .any(|poll| poll.harness == Some(HarnessState::Idle))
    })
    .await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Done);
    assert!(harness_ok.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        polls_after_completion(&calls)
            .iter()
            .all(|poll| !reports_broken_harness(poll)),
        "a healthy run must stop advertising the broken harness: {calls:?}"
    );
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
async fn job_whose_provider_goes_unusable_in_gateway_health_fails_as_provider() {
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
    agent.provider_health_watch_period = Duration::from_millis(100);
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
        "90s from the gateway plus the buffer"
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
    agent.provider_health_watch_period = Duration::from_millis(40);
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
    spec.source.git_ref = "no-such-ref".to_owned();
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
async fn a_staged_tree_left_by_an_earlier_run_is_cleared_at_start() {
    let f = fixture();
    let job_id = JobId::new();
    let controller = assigned_controller(&job_id, test_spec(&f.repo_dir.to_string_lossy()));
    // The registry is in memory, so a restart forgets a tree an earlier run staged; it carries the
    // staging marker, unlike the plain directory in the test above.
    let leftover = f.work_dir.join(job_id.as_str());
    std::fs::create_dir_all(leftover.join(".git")).unwrap();
    std::fs::write(leftover.join(".git").join("lu-staged"), b"").unwrap();
    std::fs::write(leftover.join("stale.txt"), b"left behind").unwrap();
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("echo working; echo added >> README.md"),
    );
    let calls = run_job(&controller, agent, has_complete).await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Done, "{done:?}");
    assert!(calls.iter().any(|c| matches!(c, Call::Start { .. })));
    assert!(!leftover.join("stale.txt").exists());
}

#[tokio::test]
async fn a_spawning_job_is_refused_when_the_harness_shares_the_agent_uid() {
    let f = fixture();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.allow_spawn = true;
    let controller = assigned_controller(&job_id, spec);
    let mut agent = fast_agent(&f.work_dir, controller.clone(), sh("echo should not run"));
    agent.allow_shared_uid_scope = false;
    let calls = run_job(&controller, agent, has_complete).await;

    let done = completions(&calls);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Conflict));
    let summary = done[0].summary.clone().unwrap_or_default();
    assert!(summary.contains("harness_uid"), "{summary}");
    assert!(!log_text(&calls).contains("should not run"));
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
async fn heartbeats_carry_the_providers_the_local_gateway_reports() {
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
    agent.provider_health_probe = Some((
        format!("http://127.0.0.1:{port}/v1/health"),
        Arc::from("probe-token"),
    ));
    let (stop, handle) = run_agent_with_shutdown(agent);
    let deadline = Instant::now() + Duration::from_secs(5);
    let carried = loop {
        let found = controller.check(|calls| {
            calls.iter().find_map(|call| match call {
                Call::Poll(request) if !request.providers.is_empty() => {
                    Some(request.as_ref().clone())
                }
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
    assert!(
        carried.provider_health.is_some(),
        "the document is forwarded too"
    );
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}

#[tokio::test]
async fn heartbeats_carry_workspace_counters_and_listing() {
    let f = fixture();
    let controller = Arc::new(FakeController::new());
    for _ in 0..200 {
        controller.push_poll(Ok(Vec::new()));
    }
    let agent = make_agent(&f.work_dir, controller.clone(), sh("true"));
    let job = JobId::new();
    let path = agent.workspaces.path_for(&job);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("blob.txt"), vec![b'x'; 32]).unwrap();
    agent.workspaces.register(&job, 1, WorkspacePolicy::Keep);
    let (stop, handle) = run_agent_with_shutdown(agent);
    let deadline = Instant::now() + Duration::from_secs(5);
    let carried = loop {
        let found = controller.check(|calls| {
            calls.iter().find_map(|call| match call {
                Call::Poll(request) if request.workspace_count > 0 => {
                    Some(request.as_ref().clone())
                }
                _ => None,
            })
        });
        if let Some(request) = found {
            break request;
        }
        assert!(Instant::now() < deadline, "no heartbeat carried workspaces");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(carried.workspace_bytes >= 32);
    assert_eq!(carried.workspace_count, 1);
    assert_eq!(carried.workspaces.len(), 1);
    assert_eq!(carried.workspaces[0].job_key, job.to_string());
    assert_eq!(carried.workspaces[0].state, WorkspaceState::Running);
    assert_eq!(carried.workspaces[0].policy, WorkspacePolicy::Keep);
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
    spec.source.base = Some("a".repeat(40));
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
    spec.source.base = Some("a".repeat(40));
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
    spec.source.base = Some("a".repeat(40));
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

#[tokio::test]
async fn heartbeats_use_probe_results_without_waiting_for_the_command() {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let mut agent = make_agent(&work_dir, Arc::new(FakeController::new()), success_harness);
    let probes = Arc::new(super::probes::Probes::new(vec![lu_config::DetectConfig {
        label: "slow-probe".to_owned(),
        command: vec!["/bin/sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()],
        timeout_secs: 5,
        every_secs: 60,
    }]));
    agent.labels = Arc::new(
        super::labels::LabelSource::new(Vec::new(), Vec::new()).with_probes(probes.results()),
    );
    agent.probes = Arc::clone(&probes);
    let tasks = agent.probes.spawn_all();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    let request = agent.poll_request(lu_proto::HarnessState::Idle, 0);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(request.version.as_deref(), Some(crate::TING_VERSION));
    assert!(!request.labels.iter().any(|label| label == "slow-probe"));
    assert!(request.label_sources.is_empty());
    for task in tasks {
        task.abort();
    }

    agent.probes.results().set("slow-probe", true);
    let request = agent.poll_request(lu_proto::HarnessState::Idle, 0);
    assert!(request.labels.iter().any(|label| label == "slow-probe"));
    assert_eq!(
        request.label_sources.get("slow-probe").map(String::as_str),
        Some(lu_proto::LABEL_SOURCE_PROBED)
    );
}

#[test]
fn heartbeats_publish_only_configured_healthy_provider_pairs() {
    use lu_proto::{ProviderModel, ProviderState, ProviderStatus};

    let config = vec![
        lu_config::ProviderModelConfig {
            provider: "p1".to_owned(),
            model: "m1".to_owned(),
        },
        lu_config::ProviderModelConfig {
            provider: "p2".to_owned(),
            model: "m2".to_owned(),
        },
        lu_config::ProviderModelConfig {
            provider: "p3".to_owned(),
            model: "m3".to_owned(),
        },
    ];
    let providers = vec![
        ProviderStatus {
            name: "p1".to_owned(),
            state: ProviderState::Cooling,
            soonest_secs: Some(30),
        },
        ProviderStatus {
            name: "p2".to_owned(),
            state: ProviderState::Ok,
            soonest_secs: None,
        },
    ];
    assert_eq!(
        super::offered_provider_models(&config, &providers),
        vec![ProviderModel {
            provider: "p2".to_owned(),
            model: "m2".to_owned(),
        }]
    );
}

/// A clock that advances one virtual second per call. The stuck watcher and the output reader
/// are the only callers during these tests, so virtual time tracks watcher samples without
/// sleeping through the production thresholds.
#[derive(Debug)]
struct SteppingClock {
    start: Instant,
    step_millis: u64,
    calls: AtomicU64,
}

impl SteppingClock {
    fn new(step_secs: u64) -> Self {
        Self {
            start: Instant::now(),
            step_millis: step_secs.saturating_mul(1_000),
            calls: AtomicU64::new(0),
        }
    }
}

impl super::proc::Clock for SteppingClock {
    fn now(&self) -> Instant {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.start + Duration::from_millis(self.step_millis.saturating_mul(call))
    }
}

/// A harness that produces no output and no CPU once started.
fn idle_harness(_task_file: &Path) -> CommandSpec {
    CommandSpec::new("sh").arg("-c").arg("sleep 10")
}

/// A harness that goes idle, emits one line, then exits before the stuck interval is reached
/// again. The line must reset the stuck watcher's timer.
fn activity_harness(_task_file: &Path) -> CommandSpec {
    CommandSpec::new("sh")
        .arg("-c")
        .arg("sleep 0.02; echo alive; sleep 0.03")
}

/// A harness that finishes while still inside the stuck interval.
fn short_harness(_task_file: &Path) -> CommandSpec {
    CommandSpec::new("sh").arg("-c").arg("sleep 0.02")
}

async fn wait_for_completion(controller: &FakeController) -> Completion {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let found = controller.check(|calls| {
                calls.iter().find_map(|call| match call {
                    Call::Complete(completion) => Some(completion.clone()),
                    _ => None,
                })
            });
            if let Some(completion) = found {
                return completion;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("job did not complete")
}

/// Runs one job with a lowered observation threshold and virtual clock. `after_override` stands
/// in for a sub-minute policy threshold; job fields themselves must stay in the valid range.
async fn run_stuck_case(
    mut spec: JobSpec,
    harness: fn(&Path) -> CommandSpec,
    default_policy: StuckPolicy,
    default_after_secs: u64,
    step_secs: u64,
    after_override: Option<u64>,
) -> (Completion, Vec<Call>, Result<(), AgentError>) {
    let tmp = TempDir::new().unwrap();
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir(&repo_dir).unwrap();
    create_git_repo(&repo_dir);
    spec.repo = repo_dir.to_string_lossy().into_owned();
    spec.source.repo = repo_dir.to_string_lossy().into_owned();

    let controller = controller_with_assign(JobId::new(), EPOCH, spec);
    let mut agent = make_agent(&work_dir, controller.clone(), harness);
    agent.default_on_stuck = default_policy;
    agent.default_stuck_after_secs = default_after_secs;
    let agent = agent.with_stuck_timing_for_tests(
        2,
        Duration::from_millis(2),
        after_override,
        Arc::new(SteppingClock::new(step_secs)),
    );
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });
    let completion = wait_for_completion(&controller).await;
    let _ = shutdown_tx.send(());
    let result = handle.await.unwrap();
    (completion, controller.take_calls(), result)
}

fn assert_stuck_artifacts(calls: &[Call]) {
    for name in [ArtifactName::DiffPatch, ArtifactName::ReportJson] {
        assert!(
            calls.iter().any(|call| matches!(
                call,
                Call::PutArtifact { name: actual, .. } if *actual == name
            )),
            "missing {name} artifact"
        );
    }
}

fn assert_stuck_summary(completion: &Completion, secs: u64) {
    assert_eq!(
        completion.summary.as_deref(),
        Some(format!("stuck for {secs}s, stopped").as_str())
    );
}

#[tokio::test]
async fn stuck_cancel_policy_stops_the_harness_and_fails_execution() {
    let mut spec = test_spec("");
    spec.on_stuck = Some(StuckPolicy::Cancel);
    spec.stuck_after_secs = Some(60);
    let (completion, calls, result) =
        run_stuck_case(spec, idle_harness, StuckPolicy::None, 1_800, 1, Some(5)).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        completion.outcome,
        Outcome::Failed(FailureClass::Execution),
        "{completion:?}"
    );
    assert_eq!(
        completion.stuck_mode,
        Some(StuckPolicy::Cancel),
        "{completion:?}"
    );
    assert_stuck_summary(&completion, 5);
    assert_stuck_artifacts(&calls);
}

#[tokio::test]
async fn stuck_retry_policy_fails_with_node_class() {
    let mut spec = test_spec("");
    spec.on_stuck = Some(StuckPolicy::Retry);
    spec.stuck_after_secs = Some(60);
    let (completion, calls, result) =
        run_stuck_case(spec, idle_harness, StuckPolicy::None, 1_800, 1, Some(4)).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(completion.outcome, Outcome::Failed(FailureClass::Node));
    assert_eq!(completion.stuck_mode, Some(StuckPolicy::Retry));
    assert_stuck_summary(&completion, 4);
    assert_stuck_artifacts(&calls);
}

#[tokio::test]
async fn output_resets_the_stuck_timer() {
    let mut spec = test_spec("");
    spec.on_stuck = Some(StuckPolicy::Retry);
    spec.stuck_after_secs = Some(60);
    let (completion, _, result) =
        run_stuck_case(spec, activity_harness, StuckPolicy::None, 1_800, 1, None).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(completion.outcome, Outcome::Done);
    assert_eq!(completion.stuck_mode, None);
}

#[tokio::test]
async fn job_finishing_before_the_threshold_is_not_stopped() {
    let mut spec = test_spec("");
    spec.on_stuck = Some(StuckPolicy::Cancel);
    spec.stuck_after_secs = Some(60);
    let (completion, _, result) =
        run_stuck_case(spec, short_harness, StuckPolicy::None, 1_800, 1, None).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(completion.outcome, Outcome::Done);
    assert_eq!(completion.stuck_mode, None);
}

#[tokio::test]
async fn agent_default_applies_when_the_job_omits_stuck_fields() {
    let spec = test_spec("");
    let (completion, calls, result) =
        run_stuck_case(spec, idle_harness, StuckPolicy::Cancel, 60, 1, Some(5)).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(completion.outcome, Outcome::Failed(FailureClass::Execution));
    assert_eq!(completion.stuck_mode, Some(StuckPolicy::Cancel));
    assert_stuck_summary(&completion, 5);
    assert_stuck_artifacts(&calls);
}

#[tokio::test]
async fn explicit_job_none_disables_the_agent_default() {
    let mut spec = test_spec("");
    spec.on_stuck = Some(StuckPolicy::None);
    spec.stuck_after_secs = Some(60);
    let (completion, _, result) =
        run_stuck_case(spec, short_harness, StuckPolicy::Cancel, 60, 1, Some(5)).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(completion.outcome, Outcome::Done);
    assert_eq!(completion.stuck_mode, None);
}

#[tokio::test]
async fn job_override_wins_over_the_agent_default() {
    let mut spec = test_spec("");
    spec.on_stuck = Some(StuckPolicy::Retry);
    spec.stuck_after_secs = Some(60);
    let (completion, _, result) =
        run_stuck_case(spec, idle_harness, StuckPolicy::Cancel, 100, 30, None).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(completion.outcome, Outcome::Failed(FailureClass::Node));
    assert_eq!(completion.stuck_mode, Some(StuckPolicy::Retry));
    assert_stuck_summary(&completion, 60);
}

// --- Stacked chains -------------------------------------------------------------------------

fn commit_file(repo: &Path, path: &str, content: &str, message: &str) -> String {
    std::fs::write(repo.join(path), content).unwrap();
    git(repo, &["add", path]);
    git(repo, &["commit", "-q", "-m", message]);
    head_commit(repo)
}

fn reported_staged_commits(calls: &[Call]) -> Vec<String> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::StagedCommit { commit, .. } => Some(commit.clone()),
            _ => None,
        })
        .collect()
}

fn merge_base_calls(calls: &[Call]) -> Vec<Vec<String>> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::MergeBase { commits, .. } => Some(commits.clone()),
            _ => None,
        })
        .collect()
}

fn hex_commit(digit: char) -> String {
    digit.to_string().repeat(40)
}

#[tokio::test]
async fn two_link_chain_inherits_the_dependency_base_after_the_branch_moves() {
    let f = fixture();
    let base = f.base_commit.clone();
    let diff = make_diff(&f.repo_dir, "README.md", "test\n", "test\ndep\n");
    // The branch moves after the dependency was staged: a fresh checkout would miss `dep`.
    let moved = commit_file(&f.repo_dir, "README.md", "moved\n", "branch moved");
    assert_ne!(moved, base);

    let job_id = JobId::new();
    let dep = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![dependency(&dep, &diff, &base)]));
    controller.push_dep_diff(Ok(diff));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^dep$' README.md && ! grep -q '^moved$' README.md"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    assert_eq!(reported_staged_commits(&calls), vec![base]);
    assert!(merge_base_calls(&calls).is_empty());
}

#[tokio::test]
async fn three_link_chain_inherits_one_base_for_every_patch() {
    let f = fixture();
    let base = f.base_commit.clone();
    let first = make_diff(&f.repo_dir, "README.md", "test\n", "test\none\n");
    let second = make_diff(&f.repo_dir, "README.md", "test\n", "test\none\ntwo\n");
    let third = make_diff(
        &f.repo_dir,
        "README.md",
        "test\n",
        "test\none\ntwo\nthree\n",
    );

    let deps: Vec<JobId> = (0..3).map(|_| JobId::new()).collect();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = deps.clone();
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![
        dependency(&deps[0], &first, &base),
        dependency(&deps[1], &second, &base),
        dependency(&deps[2], &third, &base),
    ]));
    controller.push_dep_diff(Ok(first));
    controller.push_dep_diff(Ok(second));
    controller.push_dep_diff(Ok(third));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^one$' README.md && grep -q '^two$' README.md && grep -q '^three$' README.md"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    assert_eq!(reported_staged_commits(&calls), vec![base]);
    let report: serde_json::Value =
        serde_json::from_slice(&artifact(&calls, ArtifactName::ReportJson)).unwrap();
    assert_eq!(
        report["applied_dependencies"],
        json!(deps.iter().map(JobId::to_string).collect::<Vec<_>>())
    );
}

#[tokio::test]
async fn diamond_with_agreeing_bases_needs_no_merge_base_call() {
    let mut f = fixture();
    std::fs::write(f.repo_dir.join("a.txt"), "a\n").unwrap();
    std::fs::write(f.repo_dir.join("b.txt"), "b\n").unwrap();
    git(&f.repo_dir, &["add", "a.txt", "b.txt"]);
    git(&f.repo_dir, &["commit", "-q", "-m", "files"]);
    f.base_commit = head_commit(&f.repo_dir);
    let base = f.base_commit.clone();
    let first = make_diff(&f.repo_dir, "a.txt", "a\n", "a-one\n");
    let second = make_diff(&f.repo_dir, "b.txt", "b\n", "b-two\n");

    let dep1 = JobId::new();
    let dep2 = JobId::new();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep1.clone(), dep2.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![
        dependency(&dep1, &first, &base),
        dependency(&dep2, &second, &base),
    ]));
    controller.push_dep_diff(Ok(first));
    controller.push_dep_diff(Ok(second));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^a-one$' a.txt && grep -q '^b-two$' b.txt"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    assert_eq!(reported_staged_commits(&calls), vec![base]);
    assert!(merge_base_calls(&calls).is_empty());
}

/// Builds two branch tips over the fixture's base. The branch commits touch `marker.txt` only,
/// so patches against files from the common base still apply after a merge-base checkout.
fn diverged_deps(f: &mut Fixture) -> (String, String, Vec<u8>, Vec<u8>) {
    std::fs::write(f.repo_dir.join("a.txt"), "a\n").unwrap();
    std::fs::write(f.repo_dir.join("b.txt"), "b\n").unwrap();
    git(&f.repo_dir, &["add", "a.txt", "b.txt"]);
    git(&f.repo_dir, &["commit", "-q", "-m", "files"]);
    f.base_commit = head_commit(&f.repo_dir);
    let base = f.base_commit.clone();
    let first = make_diff(&f.repo_dir, "a.txt", "a\n", "a-one\n");
    let second = make_diff(&f.repo_dir, "b.txt", "b\n", "b-two\n");

    git(&f.repo_dir, &["checkout", "-q", "-b", "left"]);
    let left = commit_file(&f.repo_dir, "marker.txt", "left\n", "left branch");
    git(&f.repo_dir, &["checkout", "-q", "-b", "right", &base]);
    let right = commit_file(&f.repo_dir, "marker.txt", "right\n", "right branch");
    git(&f.repo_dir, &["checkout", "-q", "main"]);
    (left, right, first, second)
}

#[tokio::test]
async fn diamond_with_disagreeing_bases_uses_the_controllers_merge_base() {
    let mut f = fixture();
    let (left, right, first, second) = diverged_deps(&mut f);
    let base = f.base_commit.clone();

    let dep1 = JobId::new();
    let dep2 = JobId::new();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep1.clone(), dep2.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![
        dependency(&dep1, &first, &left),
        dependency(&dep2, &second, &right),
    ]));
    controller.push_merge_base(Ok(base.clone()));
    controller.push_dep_diff(Ok(first));
    controller.push_dep_diff(Ok(second));
    let agent = fast_agent(
        &f.work_dir,
        controller.clone(),
        sh("grep -q '^a-one$' a.txt && grep -q '^b-two$' b.txt && ! test -e marker.txt"),
    );
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    assert_eq!(completions(&calls)[0].outcome, Outcome::Done);
    assert_eq!(merge_base_calls(&calls), vec![vec![left, right]]);
    assert_eq!(reported_staged_commits(&calls), vec![base]);
}

#[tokio::test]
async fn diamond_merge_base_conflict_names_both_commits_and_every_hunk() {
    let mut f = fixture();
    let (left, right, _, _) = diverged_deps(&mut f);
    let base = f.base_commit.clone();
    let first = make_diff(&f.repo_dir, "README.md", "test\n", "one\n");
    let second = make_diff(&f.repo_dir, "README.md", "test\n", "two\n");

    let dep1 = JobId::new();
    let dep2 = JobId::new();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep1.clone(), dep2.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![
        dependency(&dep1, &first, &left),
        dependency(&dep2, &second, &right),
    ]));
    controller.push_merge_base(Ok(base.clone()));
    controller.push_dep_diff(Ok(first));
    controller.push_dep_diff(Ok(second));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo harness-ran"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Conflict));
    let summary = done[0].summary.as_deref().unwrap_or("");
    assert!(summary.contains(&left), "{summary}");
    assert!(summary.contains(&right), "{summary}");
    assert!(summary.contains("conflicted files: README.md"), "{summary}");
    assert!(summary.contains("conflicted hunks:"), "{summary}");
    assert!(
        summary.contains("base moved since dependency was staged: yes"),
        "{summary}"
    );
    assert_eq!(reported_staged_commits(&calls), vec![base]);
}

#[tokio::test]
async fn a_rejected_staged_commit_stops_before_dependency_patches() {
    let f = fixture();
    let base = f.base_commit.clone();
    let diff = make_diff(&f.repo_dir, "README.md", "test\n", "test\ndep\n");
    let dep = JobId::new();
    let job_id = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep.clone()];
    let controller = assigned_controller(&job_id, spec);
    controller.push_deps(Ok(vec![dependency(&dep, &diff, &base)]));
    controller.push_staged_commit(Err(ApiError::Conflict { length: 7 }));
    controller.push_dep_diff(Ok(diff));
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo harness-ran"));
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(
        done[0].outcome,
        Outcome::Failed(FailureClass::Execution),
        "{done:?}"
    );
    let summary = done[0].summary.as_deref().unwrap_or("");
    assert!(summary.contains(&base), "{summary}");
    assert!(summary.contains("rejected"), "{summary}");
    assert_eq!(reported_staged_commits(&calls), vec![base]);
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, Call::DepDiff { .. } | Call::Start { .. })),
        "no patches or harness may run after a rejected staged commit: {calls:?}"
    );
}

#[tokio::test]
async fn job_cannot_widen_the_configured_stack_policy() {
    let f = fixture();
    let job_id = JobId::new();
    let dep = JobId::new();
    let mut spec = test_spec(&f.repo_dir.to_string_lossy());
    spec.after = vec![dep.clone()];
    spec.stack_union_paths = vec!["*.md".to_owned()];
    let controller = assigned_controller(&job_id, spec);
    let agent = fast_agent(&f.work_dir, controller.clone(), sh("echo harness-ran"));
    let mut agent = agent;
    agent.stack = Some(StackConfig {
        union_paths: vec!["CHANGELOG.md".to_owned()],
        base_paths: Vec::new(),
    });
    let calls = run_job(
        &controller,
        agent,
        job_finished(f.work_dir.join(job_id.as_str())),
    )
    .await;

    let done = completions(&calls);
    assert_eq!(done[0].outcome, Outcome::Failed(FailureClass::Conflict));
    let summary = done[0].summary.as_deref().unwrap_or("");
    assert!(summary.contains("widens"), "{summary}");
    assert!(summary.contains("*.md"), "{summary}");
    assert!(
        !calls.iter().any(|call| matches!(
            call,
            Call::JobBase { .. } | Call::StagedCommit { .. } | Call::Start { .. }
        )),
        "a widened policy must fail before staging: {calls:?}"
    );
}

#[test]
fn conflict_summary_reports_files_hunks_movement_and_covering_globs() {
    let policy = StackPolicy {
        union_paths: vec!["CHANGELOG.md".to_owned(), "docs/**".to_owned()],
        base_paths: vec!["Cargo.lock".to_owned()],
    };
    let paths = vec!["CHANGELOG.md".to_owned(), "docs/notes.md".to_owned()];
    let hunks = vec!["CHANGELOG.md@12".to_owned(), "docs/notes.md@3".to_owned()];
    let summary = dependency_conflict_summary(
        "input",
        "dep",
        &paths,
        &hunks,
        Some("hunk at line 12"),
        &ConflictContext {
            staged_base: Some(&hex_commit('a')),
            dependency_base: Some(&hex_commit('b')),
            stack_policy: Some(&policy),
            disagreement: Some("dependency staged commits disagreed (left, right)"),
        },
    );
    for expected in [
        "CHANGELOG.md",
        "docs/notes.md",
        "CHANGELOG.md@12",
        "docs/notes.md@3",
        "base moved since dependency was staged: yes",
        "dependency staged commits disagreed (left, right)",
        "union globs covering conflicts: CHANGELOG.md, docs/**",
        "base globs covering conflicts: none",
        "hunk at line 12",
    ] {
        assert!(
            summary.contains(expected),
            "missing {expected:?}: {summary}"
        );
    }

    let no_policy = dependency_conflict_summary(
        "input",
        "dep",
        &paths,
        &hunks,
        None,
        &ConflictContext {
            staged_base: Some(&hex_commit('a')),
            dependency_base: Some(&hex_commit('a')),
            stack_policy: None,
            disagreement: None,
        },
    );
    assert!(
        no_policy.contains("base moved since dependency was staged: no"),
        "{no_policy}"
    );
    assert!(
        no_policy.contains("union globs covering conflicts: none"),
        "{no_policy}"
    );
    assert!(
        no_policy.contains("base globs covering conflicts: none"),
        "{no_policy}"
    );
}

#[test]
fn disk_percentage_uses_work_filesystem_totals() {
    let host = |free, total| HostStats {
        load1: 0.0,
        load5: 0.0,
        load15: 0.0,
        mem_total_kb: 0,
        mem_available_kb: 0,
        swap_used_kb: 0,
        disk_free_bytes: Some(free),
        disk_total_bytes: Some(total),
        uptime_secs: 0,
    };
    assert_eq!(disk_free_pct(None), None);
    assert_eq!(disk_free_pct(Some(&host(10, 0))), None);
    assert_eq!(disk_free_pct(Some(&host(1, 4))), Some(25));
    assert_eq!(disk_free_pct(Some(&host(u64::MAX, 1))), Some(100));
}
