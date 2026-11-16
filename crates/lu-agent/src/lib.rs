//! The node-side agent loop.
//!
//! The agent polls its controller, runs one job at a time in a fresh checkout, streams the
//! harness output, renews its lease, and completes the job. Controller failures are reported as
//! errors only when the agent cannot continue; a broken log stream is never allowed to kill a
//! running job.

mod health;
mod logs;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lu_client::{ApiError, Controller};
use lu_config::AgentConfig;
use lu_proto::{
    printable, truncate_chars, AgentEvent, ArtifactName, Completion, FailureClass, HarnessState,
    Heartbeat, JobId, JobSpec, NodeName, Outcome, MAX_DIFF_BYTES, MAX_SUMMARY_BYTES,
};
use lu_staging::{Source, StageRequest, StagedDir, Staging};
use lu_supervise::{CommandSpec, Limits, Outcome as SuperviseOutcome, Supervisor};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use logs::{upload_loop, LogUpload, Tail};

/// How often a running job asks the controller to renew its lease. The controller's default
/// lease is 60 seconds; 20 seconds leaves room for one lost request.
pub const RENEW_PERIOD: Duration = Duration::from_secs(20);

/// How long an idle poll waits for work. The controller caps this at 30 seconds.
pub const POLL_WAIT_SECS: u64 = 25;

/// Largest checkout we are willing to produce output from. A harness beyond this is failed
/// rather than allowed to consume unbounded memory.
const MAX_HARNESS_OUTPUT: u64 = lu_proto::MAX_LOG_BYTES;

/// Time between the terminate signal and the forced stop signal.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Output quoted on a completion summary. Kept small and independent of the log cap.
const SUMMARY_TAIL_BYTES: usize = 16 * 1024;

/// The command builder is a seam for tests; production always builds the opencode command.
type CommandBuilder =
    dyn Fn(&Path, Option<&str>, Option<&str>, &str) -> Result<CommandSpec, String> + Send + Sync;

/// A fatal agent failure. Job-level failures are completed through the controller instead.
#[derive(Debug)]
pub enum AgentError {
    /// The configuration cannot produce a working agent.
    Config(String),
    /// The controller rejected the agent token.
    Unauthorized,
    /// The controller refused the agent's scope.
    Forbidden,
    /// The controller could not be used any more.
    Controller(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(message) => write!(f, "agent configuration: {message}"),
            Self::Unauthorized => f.write_str("the controller rejected the agent token"),
            Self::Forbidden => f.write_str("the controller refused this agent's scope"),
            Self::Controller(message) => write!(f, "controller failure: {message}"),
        }
    }
}

impl std::error::Error for AgentError {}

/// The outcome of one job from the agent's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobEnd {
    /// The job was completed (done or failed) or abandoned after a non-fatal error.
    Finished,
    /// The controller asked the agent to stop; nothing was completed.
    Cancelled,
    /// The lease moved on; nothing was completed.
    Stale,
    /// Shutdown arrived while the job was running.
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
enum Control {
    Cancel,
    Stale,
    Unauthorized,
    Forbidden,
}

#[derive(Debug, Clone, Copy)]
enum UploadFault {
    Stale,
    Unauthorized,
    Forbidden,
}

/// Aborts a task when dropped, so a background reader never outlives the agent loop.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The node agent. One instance runs one job at a time.
pub struct Agent {
    node: NodeName,
    controller: Arc<dyn Controller>,
    staging: Staging,
    work_dir: PathBuf,
    heartbeat_secs: u64,
    opencode_server: Option<String>,
    /// The group that may write each job's working tree, resolved once at start.
    workspace_gid: Option<u32>,
    /// The opencode server password, kept out of `Debug` and logs.
    opencode_password: Option<Arc<str>>,
    /// The local Reiki's health URL and token, when configured.
    reiki_probe: Option<(String, Arc<str>)>,
    /// The last health read from it, sent with every heartbeat.
    health: health::Shared,
    harness: String,
    harness_wrapper: Vec<String>,
    builder: Arc<CommandBuilder>,
}

impl Agent {
    /// Builds an agent from the `[agent]` configuration. The work directory is created with
    /// mode 0700 when it does not exist; the token is loaded by the caller and never reaches
    /// this type.
    pub fn new(config: AgentConfig, controller: Arc<dyn Controller>) -> Result<Self, AgentError> {
        let node = NodeName::parse(&config.name)
            .map_err(|error| AgentError::Config(format!("name: {error}")))?;
        let work_dir = PathBuf::from(&config.work_dir);
        create_work_dir(&work_dir)?;
        let staging = Staging::new(&work_dir)
            .map_err(|error| AgentError::Config(format!("work_dir: {error}")))?;
        let workspace_gid = match config.workspace_group.as_deref() {
            Some(group) => Some(
                lu_staging::resolve_group(group)
                    .map_err(|error| AgentError::Config(format!("workspace_group: {error}")))?,
            ),
            None => None,
        };
        let opencode_password = match config.opencode_password_file.as_deref() {
            Some(file) => {
                let text = lu_config::read_private_file(
                    "[agent] opencode_password_file",
                    Path::new(file),
                    4096,
                )
                .map_err(|error| AgentError::Config(error.to_string()))?;
                let password = text.trim_end_matches(['\r', '\n']);
                if password.is_empty() || password.chars().any(char::is_control) {
                    return Err(AgentError::Config(
                        "opencode_password_file must hold one line of text".to_owned(),
                    ));
                }
                Some(Arc::from(password))
            }
            None => None,
        };
        let reiki_probe = match (
            config.reiki_health_url.as_deref(),
            config.reiki_token_file.as_deref(),
        ) {
            (Some(url), Some(file)) => {
                let text =
                    lu_config::read_private_file("[agent] reiki_token_file", Path::new(file), 4096)
                        .map_err(|error| AgentError::Config(error.to_string()))?;
                let token = text.trim_end_matches(['\r', '\n']);
                if token.is_empty() || token.chars().any(char::is_control) {
                    return Err(AgentError::Config(
                        "reiki_token_file must hold one line of text".to_owned(),
                    ));
                }
                Some((url.to_owned(), Arc::from(token)))
            }
            _ => None,
        };
        Ok(Self {
            node,
            controller,
            staging,
            work_dir,
            heartbeat_secs: config.heartbeat_secs,
            opencode_server: config.opencode_server,
            workspace_gid,
            opencode_password,
            reiki_probe,
            health: health::Shared::default(),
            harness: config.harness,
            harness_wrapper: config.harness_wrapper,
            builder: Arc::new(|task_file, model, server, message| {
                lu_supervise::opencode_command(task_file, model, server, message)
                    .map_err(|error| error.to_string())
            }),
        })
    }

    /// The node name this agent presents. Useful for logging and tests.
    /// Replaces the command that runs a job. For tests that need a scripted harness; the
    /// production path always builds the opencode command.
    #[doc(hidden)]
    pub fn with_command_builder<F>(mut self, builder: F) -> Self
    where
        F: Fn(&Path, Option<&str>, Option<&str>, &str) -> Result<CommandSpec, String>
            + Send
            + Sync
            + 'static,
    {
        self.builder = Arc::new(builder);
        self
    }

    pub fn node(&self) -> &NodeName {
        &self.node
    }

    /// The work root this agent owns.
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Runs until `shutdown` resolves. A shutdown while a job is running stops the harness,
    /// reports a node failure, and resolves successfully.
    pub async fn run<F>(self, shutdown: F) -> Result<(), AgentError>
    where
        F: Future<Output = ()> + Send,
    {
        let mut shutdown = Box::pin(shutdown);
        let mut backoff = Backoff::new();
        // Ends with the agent: the guard aborts the task when `run` returns.
        let _health_task = self.reiki_probe.as_ref().map(|(url, token)| {
            AbortOnDrop(health::spawn_refresher(
                url.clone(),
                Arc::clone(token),
                Arc::clone(&self.health),
            ))
        });
        info!(node = %self.node, work_dir = %self.work_dir.display(), "agent started");
        loop {
            let request = self.poll_request(HarnessState::Idle, POLL_WAIT_SECS);
            let polled = {
                let poll = self.controller.poll(&request);
                tokio::select! {
                    biased;
                    () = shutdown.as_mut() => return Ok(()),
                    result = poll => result,
                }
            };
            match polled {
                Ok(events) => {
                    if events.is_empty() {
                        backoff.reset();
                    }
                    for event in events {
                        match event {
                            AgentEvent::Assign {
                                job_id,
                                epoch,
                                spec,
                            } => {
                                backoff.reset();
                                info!(job = %job_id, epoch, "assigned");
                                match self.run_job(job_id, epoch, spec, shutdown.as_mut()).await? {
                                    JobEnd::Shutdown => return Ok(()),
                                    JobEnd::Finished | JobEnd::Cancelled | JobEnd::Stale => {}
                                }
                            }
                            AgentEvent::Cancel { job_id, .. } => {
                                debug!(job = %job_id, "cancel for a job that is not running");
                            }
                            AgentEvent::Followup {
                                job_id, message, ..
                            } => {
                                debug!(
                                    job = %job_id,
                                    message = %truncate_chars(&printable(&message), 200),
                                    "followup ignored"
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Some(fatal) = fatal_error(&error) {
                        return Err(fatal);
                    }
                    let delay = backoff.next();
                    warn!(
                        error = %printable(&error.to_string()),
                        delay_ms = delay.as_millis(),
                        "poll failed; backing off"
                    );
                    tokio::select! {
                        biased;
                        () = shutdown.as_mut() => return Ok(()),
                        () = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    fn poll_request(&self, harness: HarnessState, wait_secs: u64) -> lu_client::PollRequest {
        let current = health::snapshot(&self.health);
        let beat = Heartbeat {
            node: self.node.clone(),
            harness,
            providers: current.providers,
            reiki: current.reiki,
        };
        let mut request = lu_client::PollRequest::from(beat);
        request.wait_secs = Some(wait_secs);
        request
    }

    async fn run_job<F>(
        &self,
        job_id: JobId,
        epoch: u64,
        spec: JobSpec,
        mut shutdown: Pin<&mut F>,
    ) -> Result<JobEnd, AgentError>
    where
        F: Future<Output = ()> + Send,
    {
        let started = Instant::now();
        // A harness in another account cannot read `.git`, so its task file sits beside the tree.
        let shared_task = self
            .workspace_gid
            .map(|gid| (self.work_dir.join(format!("{job_id}.task.md")), gid));
        let _cleanup = Cleanup {
            staging: &self.staging,
            job_id: job_id.to_string(),
            task_file: shared_task.as_ref().map(|(path, _)| path.clone()),
        };
        if let Err(error) = spec.validate() {
            let summary = format!("invalid job spec: {error}");
            self.complete_failure(&job_id, epoch, &summary, FailureClass::Execution)
                .await?;
            return Ok(JobEnd::Finished);
        }

        let source = if spec.repo.starts_with('/') {
            Source::Path(PathBuf::from(&spec.repo))
        } else {
            Source::Url(spec.repo.clone())
        };
        let staged = match self.staging.stage(StageRequest {
            job_id: job_id.to_string(),
            source,
            git_ref: spec.git_ref.clone(),
        }) {
            Ok(staged) => staged,
            Err(error) => {
                let summary = format!("staging failed: {error}");
                self.complete_failure(&job_id, epoch, &summary, FailureClass::Execution)
                    .await?;
                return Ok(JobEnd::Finished);
            }
        };

        if let Some(gid) = self.workspace_gid {
            if let Err(error) = self.staging.share_with_group(&staged, gid) {
                let summary = format!("could not share the workspace with its group: {error}");
                let _ = self.staging.clean(staged.job_id());
                self.complete_failure(&job_id, epoch, &summary, FailureClass::Execution)
                    .await?;
                return Ok(JobEnd::Finished);
            }
        }

        let task_file = match &shared_task {
            Some((path, _)) => path.clone(),
            None => staged.path().join(".git").join("lu-task.md"),
        };
        let written = std::fs::write(&task_file, spec.task.as_bytes()).and_then(|()| {
            if let Some((_, gid)) = &shared_task {
                use std::os::unix::fs::PermissionsExt;
                std::os::unix::fs::chown(&task_file, None, Some(*gid))?;
                std::fs::set_permissions(&task_file, std::fs::Permissions::from_mode(0o640))?;
            }
            Ok(())
        });
        if let Err(error) = written {
            let summary = format!("could not write the task file: {error}");
            self.complete_failure(&job_id, epoch, &summary, FailureClass::Execution)
                .await?;
            return Ok(JobEnd::Finished);
        }

        match self.controller.start(&job_id, epoch).await {
            Ok(()) => {}
            Err(error) => match error {
                ApiError::StaleEpoch => {
                    warn!(job = %job_id, "start is stale");
                    return Ok(JobEnd::Stale);
                }
                ApiError::Unauthorized => return Err(AgentError::Unauthorized),
                ApiError::Forbidden => return Err(AgentError::Forbidden),
                other => {
                    warn!(
                        job = %job_id,
                        error = %printable(&other.to_string()),
                        "start failed"
                    );
                    return Ok(JobEnd::Finished);
                }
            },
        }

        if spec.harness != "opencode" {
            let summary = format!("unsupported harness {:?}", spec.harness);
            self.finish(&Finish {
                job_id: &job_id,
                epoch,
                spec: &spec,
                staged: &staged,
                started,
                outcome: Outcome::Failed(FailureClass::Execution),
                exit_code: None,
                summary,
                cancelled: false,
            })
            .await?;
            return Ok(JobEnd::Finished);
        }
        if self.harness != "opencode" {
            warn!(
                configured = %printable(&self.harness),
                "configured harness is not opencode; jobs still use the spec harness"
            );
        }

        let command = match (self.builder)(
            &task_file,
            spec.model.as_deref(),
            self.opencode_server.as_deref(),
            "Follow the task in the provided file.",
        ) {
            Ok(command) => {
                let command = wrap_command(&self.harness_wrapper, command, staged.path());
                let command = command.cwd(staged.path());
                match &self.opencode_password {
                    Some(password) => command.env("OPENCODE_SERVER_PASSWORD", &**password),
                    None => command,
                }
            }
            Err(error) => {
                let summary = format!("could not build the harness command: {error}");
                self.finish(&Finish {
                    job_id: &job_id,
                    epoch,
                    spec: &spec,
                    staged: &staged,
                    started,
                    outcome: Outcome::Failed(FailureClass::Execution),
                    exit_code: None,
                    summary,
                    cancelled: false,
                })
                .await?;
                return Ok(JobEnd::Finished);
            }
        };

        let mut running = Supervisor::spawn(
            command,
            Limits {
                wall_timeout: Duration::from_secs(spec.timeout_secs),
                idle_output_timeout: spec.idle_timeout_secs.map(Duration::from_secs),
                max_output_bytes: MAX_HARNESS_OUTPUT,
                grace: STOP_GRACE,
            },
        );
        let mut output = running.output();

        let (log_tx, log_rx) = mpsc::channel::<Vec<u8>>(64);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (fault_tx, mut fault_rx) = mpsc::channel::<UploadFault>(1);
        let uploader = tokio::spawn(upload_loop(
            Arc::clone(&self.controller),
            job_id.clone(),
            epoch,
            log_rx,
            stop_rx,
            fault_tx,
        ));

        let (control_tx, mut control_rx) = mpsc::channel::<Control>(4);
        let control = tokio::spawn(control_loop(
            Arc::clone(&self.controller),
            self.node.clone(),
            job_id.clone(),
            epoch,
            self.heartbeat_secs,
            control_tx,
            Arc::clone(&self.health),
        ));

        let mut tail = Tail::new(SUMMARY_TAIL_BYTES);
        let mut dropped = 0u64;
        let mut end = JobEnd::Finished;

        loop {
            tokio::select! {
                biased;
                () = shutdown.as_mut() => {
                    info!(job = %job_id, "shutdown while running");
                    running.stop();
                    end = JobEnd::Shutdown;
                    break;
                }
                Some(control) = control_rx.recv() => {
                    match control {
                        Control::Cancel => {
                            info!(job = %job_id, "cancelled by the controller");
                            running.stop();
                            end = JobEnd::Cancelled;
                            break;
                        }
                        Control::Stale => {
                            warn!(job = %job_id, "lease is stale; stopping");
                            running.stop();
                            end = JobEnd::Stale;
                            break;
                        }
                        Control::Unauthorized => {
                            running.stop();
                            return Err(AgentError::Unauthorized);
                        }
                        Control::Forbidden => {
                            running.stop();
                            return Err(AgentError::Forbidden);
                        }
                    }
                }
                Some(fault) = fault_rx.recv() => {
                    match fault {
                        UploadFault::Stale => {
                            warn!(job = %job_id, "log upload lost the lease; stopping");
                            running.stop();
                            end = JobEnd::Stale;
                            break;
                        }
                        UploadFault::Unauthorized => {
                            running.stop();
                            return Err(AgentError::Unauthorized);
                        }
                        UploadFault::Forbidden => {
                            running.stop();
                            return Err(AgentError::Forbidden);
                        }
                    }
                }
                chunk = output.recv() => {
                    match chunk {
                        Some(chunk) => {
                            tail.push(&chunk.bytes);
                            for piece in logs::split_lossy(&chunk.bytes) {
                                if log_tx.try_send(piece.into_bytes()).is_err() {
                                    dropped = dropped.saturating_add(1);
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        control.abort();
        let outcome = running.wait().await;
        drop(log_tx);
        let _ = stop_tx.send(());
        let uploaded = tokio::time::timeout(Duration::from_secs(5), uploader).await;

        match end {
            JobEnd::Stale => return Ok(end),
            JobEnd::Cancelled => {
                let completion = Completion {
                    job_id: job_id.clone(),
                    epoch,
                    outcome: Outcome::Cancelled,
                    exit_code: None,
                    summary: Some(summarise("cancelled by the controller")),
                };
                match self.controller.complete(&completion).await {
                    Ok(()) | Err(ApiError::StaleEpoch) => {}
                    Err(ApiError::Unauthorized) => return Err(AgentError::Unauthorized),
                    Err(ApiError::Forbidden) => return Err(AgentError::Forbidden),
                    Err(error) => {
                        warn!(job = %job_id, %error, "could not report the cancelled job")
                    }
                }
                return Ok(end);
            }
            JobEnd::Shutdown => {
                let _ = self
                    .complete_failure(
                        &job_id,
                        epoch,
                        "agent shut down while the job was running",
                        FailureClass::Node,
                    )
                    .await;
                return Ok(JobEnd::Shutdown);
            }
            JobEnd::Finished => {}
        }

        if let Ok(Ok(LogUpload {
            fault: Some(fault), ..
        })) = uploaded
        {
            match fault {
                UploadFault::Stale => return Ok(JobEnd::Stale),
                UploadFault::Unauthorized => return Err(AgentError::Unauthorized),
                UploadFault::Forbidden => return Err(AgentError::Forbidden),
            }
        }
        if dropped > 0 {
            warn!(job = %job_id, chunks = dropped, "dropped log chunks because the upload queue was full");
        }

        let (outcome, exit_code, reason) = classify(outcome, &tail.text());
        let mut summary = tail.summary();
        if summary.is_empty() {
            summary = reason;
        }
        self.finish(&Finish {
            job_id: &job_id,
            epoch,
            spec: &spec,
            staged: &staged,
            started,
            outcome,
            exit_code,
            summary,
            cancelled: false,
        })
        .await?;
        Ok(JobEnd::Finished)
    }

    async fn complete_failure(
        &self,
        job_id: &JobId,
        epoch: u64,
        summary: &str,
        class: FailureClass,
    ) -> Result<(), AgentError> {
        let completion = Completion {
            job_id: job_id.clone(),
            epoch,
            outcome: Outcome::Failed(class),
            exit_code: None,
            summary: Some(summarise(summary)),
        };
        match self.controller.complete(&completion).await {
            Ok(()) => Ok(()),
            Err(ApiError::StaleEpoch) => Ok(()),
            Err(ApiError::Unauthorized) => Err(AgentError::Unauthorized),
            Err(ApiError::Forbidden) => Err(AgentError::Forbidden),
            Err(error) => {
                warn!(
                    job = %job_id,
                    error = %printable(&error.to_string()),
                    "completion failed"
                );
                Ok(())
            }
        }
    }

    async fn finish(&self, finish: &Finish<'_>) -> Result<JobEnd, AgentError> {
        let diff = match Staging::export_diff(finish.staged, MAX_DIFF_BYTES as usize) {
            Ok(diff) => Some(diff),
            Err(error) => {
                warn!(
                    job = %finish.job_id,
                    error = %printable(&error.to_string()),
                    "diff export failed"
                );
                None
            }
        };
        if let Some(diff) = diff {
            match self
                .controller
                .put_artifact(finish.job_id, finish.epoch, ArtifactName::DiffPatch, &diff)
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    if let Some(end) = self.artifact_end(&error, finish.job_id) {
                        return Ok(end);
                    }
                }
            }
        }

        let report = report_json(
            finish.outcome,
            finish.exit_code,
            finish.started.elapsed(),
            &finish.spec.harness,
            finish.spec.model.as_deref(),
        );
        if let Ok(report) = report {
            match self
                .controller
                .put_artifact(
                    finish.job_id,
                    finish.epoch,
                    ArtifactName::ReportJson,
                    &report,
                )
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    if let Some(end) = self.artifact_end(&error, finish.job_id) {
                        return Ok(end);
                    }
                }
            }
        }

        if finish.cancelled {
            return Ok(JobEnd::Cancelled);
        }

        let completion = Completion {
            job_id: finish.job_id.clone(),
            epoch: finish.epoch,
            outcome: finish.outcome,
            exit_code: finish.exit_code,
            summary: Some(summarise(&finish.summary)),
        };
        match self.controller.complete(&completion).await {
            Ok(()) => Ok(JobEnd::Finished),
            Err(error) => match error {
                ApiError::StaleEpoch => Ok(JobEnd::Stale),
                ApiError::Unauthorized => Err(AgentError::Unauthorized),
                ApiError::Forbidden => Err(AgentError::Forbidden),
                other => {
                    warn!(
                        job = %finish.job_id,
                        error = %printable(&other.to_string()),
                        "completion failed"
                    );
                    Ok(JobEnd::Finished)
                }
            },
        }
    }

    fn artifact_end(&self, error: &ApiError, job_id: &JobId) -> Option<JobEnd> {
        match error {
            ApiError::StaleEpoch => {
                warn!(job = %job_id, "artifact upload is stale");
                Some(JobEnd::Stale)
            }
            ApiError::Unauthorized | ApiError::Forbidden => None,
            other => {
                warn!(
                    job = %job_id,
                    error = %printable(&other.to_string()),
                    "artifact upload failed"
                );
                None
            }
        }
    }
}

/// Arguments for the common artifact-and-completion path.
struct Finish<'a> {
    job_id: &'a JobId,
    epoch: u64,
    spec: &'a JobSpec,
    staged: &'a StagedDir,
    started: Instant,
    outcome: Outcome,
    exit_code: Option<i32>,
    summary: String,
    cancelled: bool,
}

/// Removes the checkout even when the job is cancelled, the future is aborted, or it panics.
struct Cleanup<'a> {
    staging: &'a Staging,
    job_id: String,
    task_file: Option<PathBuf>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = self.staging.clean(&self.job_id);
        if let Some(task_file) = &self.task_file {
            let _ = std::fs::remove_file(task_file);
        }
    }
}

async fn control_loop(
    controller: Arc<dyn Controller>,
    node: NodeName,
    job_id: JobId,
    epoch: u64,
    heartbeat_secs: u64,
    tx: mpsc::Sender<Control>,
    shared_health: health::Shared,
) {
    let mut renew = tokio::time::interval(RENEW_PERIOD);
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    renew.tick().await;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_secs.max(1)));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = renew.tick() => {
                match controller.renew(&job_id, epoch).await {
                    Ok(false) => {}
                    Ok(true) => {
                        let _ = tx.send(Control::Cancel).await;
                        return;
                    }
                    Err(error) => {
                        match control_fault(&error) {
                            Some(Control::Stale) | Some(Control::Cancel) => {
                                let _ = tx.send(Control::Stale).await;
                                return;
                            }
                            Some(Control::Unauthorized) => {
                                let _ = tx.send(Control::Unauthorized).await;
                                return;
                            }
                            Some(Control::Forbidden) => {
                                let _ = tx.send(Control::Forbidden).await;
                                return;
                            }
                            None => {
                                warn!(
                                    job = %job_id,
                                    error = %printable(&error.to_string()),
                                    "renew failed"
                                );
                            }
                        }
                    }
                }
            }
            _ = heartbeat.tick() => {
                let current = health::snapshot(&shared_health);
                let beat = Heartbeat {
                    node: node.clone(),
                    harness: HarnessState::Busy,
                    providers: current.providers,
                    reiki: current.reiki,
                };
                let mut request = lu_client::PollRequest::from(beat);
                request.wait_secs = Some(0);
                match controller.poll(&request).await {
                    Ok(events) => {
                        for event in events {
                            match event {
                                AgentEvent::Cancel {
                                    job_id: cancelled, ..
                                } if cancelled == job_id => {
                                    let _ = tx.send(Control::Cancel).await;
                                    return;
                                }
                                AgentEvent::Assign { job_id: assigned, .. } => {
                                    warn!(job = %assigned, "assignment ignored while busy");
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(error) => {
                        match control_fault(&error) {
                            Some(Control::Stale) | Some(Control::Cancel) => {
                                let _ = tx.send(Control::Stale).await;
                                return;
                            }
                            Some(Control::Unauthorized) => {
                                let _ = tx.send(Control::Unauthorized).await;
                                return;
                            }
                            Some(Control::Forbidden) => {
                                let _ = tx.send(Control::Forbidden).await;
                                return;
                            }
                            None => {
                                warn!(
                                    job = %job_id,
                                    error = %printable(&error.to_string()),
                                    "poll failed while busy"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn control_fault(error: &ApiError) -> Option<Control> {
    match error {
        ApiError::StaleEpoch => Some(Control::Stale),
        ApiError::Unauthorized => Some(Control::Unauthorized),
        ApiError::Forbidden => Some(Control::Forbidden),
        _ => None,
    }
}

fn fatal_error(error: &ApiError) -> Option<AgentError> {
    match error {
        ApiError::Unauthorized => Some(AgentError::Unauthorized),
        ApiError::Forbidden => Some(AgentError::Forbidden),
        ApiError::Config { message } => Some(AgentError::Config(message.clone())),
        _ => None,
    }
}

/// Poll backoff: one second, doubled to thirty, with a small bounded jitter.
struct Backoff {
    next: Duration,
}

impl Backoff {
    const FIRST: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self { next: Self::FIRST }
    }

    fn reset(&mut self) {
        self.next = Self::FIRST;
    }

    fn next(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        let extra_ms = jitter_seed() % (base.as_millis() as u64 / 4 + 1);
        base + Duration::from_millis(extra_ms)
    }
}

fn jitter_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_nanos()))
        .unwrap_or(0)
}

/// Maps a supervisor outcome to the wire outcome, an exit code, and a fallback summary.
fn classify(outcome: SuperviseOutcome, tail: &str) -> (Outcome, Option<i32>, String) {
    match outcome {
        SuperviseOutcome::Exited(0) => {
            (Outcome::Done, Some(0), "harness exited cleanly".to_owned())
        }
        SuperviseOutcome::Exited(code) if provider_marker(tail) => (
            Outcome::Failed(FailureClass::Provider),
            Some(code),
            format!("provider unavailable (exit {code})"),
        ),
        SuperviseOutcome::Exited(code) => (
            Outcome::Failed(FailureClass::Execution),
            Some(code),
            format!("harness exited with {code}"),
        ),
        SuperviseOutcome::Signalled(signal) => (
            Outcome::Failed(FailureClass::Execution),
            None,
            format!("harness was killed by signal {signal}"),
        ),
        SuperviseOutcome::WallTimeout => (
            Outcome::Failed(FailureClass::Execution),
            None,
            "harness exceeded its wall-clock timeout".to_owned(),
        ),
        SuperviseOutcome::IdleTimeout => (
            Outcome::Failed(FailureClass::Execution),
            None,
            "harness produced no output before its idle timeout".to_owned(),
        ),
        SuperviseOutcome::OutputLimit => (
            Outcome::Failed(FailureClass::Execution),
            None,
            "harness produced more output than the agent accepts".to_owned(),
        ),
        SuperviseOutcome::Stopped => (
            Outcome::Failed(FailureClass::Execution),
            None,
            "harness was stopped".to_owned(),
        ),
        SuperviseOutcome::SpawnFailed(message) => (
            Outcome::Failed(FailureClass::Execution),
            None,
            format!(
                "harness could not be started: {}",
                truncate_chars(&printable(&message), 300)
            ),
        ),
    }
}

fn provider_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("every key in this stream's chain is unavailable")
        || lower.contains("quota")
        || lower.contains("http 429")
        || lower.contains("429 too many requests")
        || lower.contains("rate limit")
}

/// Caps and neutralises the completion summary. Whole lines are preferred, but the last bytes
/// are kept when one line is too long.
fn summarise(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|ch| {
            if ch == '\n' || ch == '\t' || !ch.is_control() {
                ch
            } else {
                '?'
            }
        })
        .collect();
    if clean.len() <= MAX_SUMMARY_BYTES {
        return clean;
    }
    let mut start = clean.len() - MAX_SUMMARY_BYTES;
    while !clean.is_char_boundary(start) {
        start += 1;
    }
    if let Some(newline) = clean[start..].find('\n') {
        start += newline + 1;
    }
    clean[start..].to_owned()
}

fn report_json(
    outcome: Outcome,
    exit_code: Option<i32>,
    duration: Duration,
    harness: &str,
    model: Option<&str>,
) -> Result<Vec<u8>, serde_json::Error> {
    let report = Report {
        exit_code,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        outcome,
        harness,
        model,
    };
    serde_json::to_vec(&report)
}

#[derive(serde::Serialize)]
struct Report<'a> {
    exit_code: Option<i32>,
    duration_ms: u64,
    outcome: Outcome,
    harness: &'a str,
    model: Option<&'a str>,
}

fn create_work_dir(path: &Path) -> Result<(), AgentError> {
    if path.exists() {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|error| AgentError::Config(format!("work_dir: {error}")))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(AgentError::Config(
                "work_dir must be a real directory".to_owned(),
            ));
        }
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(path)
            .map_err(|error| AgentError::Config(format!("cannot create work_dir: {error}")))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
            .map_err(|error| AgentError::Config(format!("cannot create work_dir: {error}")))
    }
}

#[cfg(test)]
mod tests;

/// Prefixes `command` with the configured wrapper, substituting `{cwd}` with the job's tree.
fn wrap_command(wrapper: &[String], command: CommandSpec, cwd: &Path) -> CommandSpec {
    let Some((program, rest)) = wrapper.split_first() else {
        return command;
    };
    let cwd = cwd.to_string_lossy();
    let mut wrapped = CommandSpec::new(program);
    for arg in rest {
        wrapped = wrapped.arg(arg.replace("{cwd}", &cwd));
    }
    wrapped = wrapped.arg(command.program.as_os_str().to_owned());
    for arg in command.args {
        wrapped = wrapped.arg(arg);
    }
    wrapped.env = command.env;
    wrapped
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    #[test]
    fn wrapper_prefixes_and_substitutes_cwd() {
        let inner = CommandSpec::new("opencode").arg("run").arg("x");
        let wrapped = wrap_command(
            &[
                "podman".into(),
                "exec".into(),
                "--workdir".into(),
                "{cwd}".into(),
                "c".into(),
            ],
            inner,
            Path::new("/w/job"),
        );
        assert_eq!(wrapped.program, Path::new("podman"));
        let args: Vec<_> = wrapped
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["exec", "--workdir", "/w/job", "c", "opencode", "run", "x"]
        );
    }

    #[test]
    fn empty_wrapper_leaves_command() {
        let wrapped = wrap_command(
            &[],
            CommandSpec::new("opencode").arg("run"),
            Path::new("/w"),
        );
        assert_eq!(wrapped.program, Path::new("opencode"));
    }
}
