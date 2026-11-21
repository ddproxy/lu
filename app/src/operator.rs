//! Operator subcommands. Every command talks to the controller through `lu-client`; this module
//! only resolves the connection, formats untrusted values, and maps the outcome to an exit code.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use lu_client::{
    ApiError, Client, JobView, ListRepos, LogPage, NodeStats, NodeView, RepoView, StatusView,
};
use lu_proto::{
    printable, truncate_chars, ArtifactName, CollectMode, FailureClass, HarnessState, JobEdit,
    JobId, JobSpec, JobState, NodeName, OnConflict, DEFAULT_TIMEOUT_SECS, LABEL_SOURCE_PROBED,
    MAX_TASK_BYTES,
};
use lu_tui::graph::{self, Graph};
use lu_tui::model::Tone;

const EXIT_ERROR: u8 = 1;
const EXIT_UNSUCCESSFUL: u8 = 3;
const EXIT_USAGE: u8 = 2;
const TOKEN_LIMIT: u64 = 8 * 1024;
const FOLLOW_WAIT_SECS: u64 = 25;
const WAIT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SUMMARY_CHARS: usize = 2000;

#[derive(Debug, clap::Args)]
pub struct SubmitArgs {
    /// Repository URL or absolute local path.
    #[arg(long)]
    pub repo: String,
    /// Git ref to check out.
    #[arg(long = "ref")]
    pub git_ref: String,
    /// Harness name.
    #[arg(long, default_value = "opencode")]
    pub harness: String,
    /// Provider name.
    #[arg(long)]
    pub provider: Option<String>,
    /// Model name.
    #[arg(long)]
    pub model: Option<String>,
    /// Alternative provider[:model] to try in order (repeatable).
    #[arg(long = "fallback", value_parser = parse_fallback)]
    pub fallbacks: Vec<lu_proto::Choice>,
    /// Job timeout in seconds.
    #[arg(long, default_value_t = lu_proto::DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Stop the harness after this many seconds with no output.
    #[arg(long, value_parser = parse_idle_timeout)]
    pub idle_timeout: Option<u64>,
    /// Only run after this job is done (repeatable).
    #[arg(long = "after", value_parser = parse_job_id)]
    pub after: Vec<JobId>,
    /// Only run after the job with this idempotency key (repeatable).
    #[arg(long = "after-key")]
    pub after_key: Vec<String>,
    /// Do not apply dependency diffs to this job's tree.
    #[arg(long)]
    pub no_stack: bool,
    /// Stage dependency artifacts under .lu/inputs: none, files, apply or both.
    #[arg(long, value_name = "MODE")]
    pub collect: Option<CollectMode>,
    /// Treat failed and cancelled dependencies as inputs when they stored a diff.
    #[arg(long)]
    pub include_failed: bool,
    /// How to handle an input that does not apply: fail, markers or skip.
    #[arg(long = "on-conflict", value_name = "MODE", default_value_t = OnConflict::Fail)]
    pub on_conflict: OnConflict,
    /// Expect an empty diff, so no `no changes` note is added when the job is done.
    #[arg(long)]
    pub allow_empty: bool,
    /// Run only on a node with this label (repeatable).
    #[arg(long = "require")]
    pub require: Vec<String>,
    /// Tolerate this node taint (repeatable).
    #[arg(long = "tolerate")]
    pub tolerate: Vec<String>,
    /// Idempotency key.
    #[arg(long)]
    pub key: Option<String>,
    /// Task text. If neither this nor --task-file is given, the task is read from stdin.
    #[arg(long, conflicts_with = "task_file")]
    pub task: Option<String>,
    /// Read the task from a file.
    #[arg(long = "task-file", conflicts_with = "task")]
    pub task_file: Option<PathBuf>,
    /// Wait for the job to finish.
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, clap::Args)]
pub struct CollectArgs {
    /// Earlier jobs to collect, as job ids or idempotency keys (repeatable).
    #[arg(required = true)]
    pub inputs: Vec<String>,
    /// Repository URL or absolute local path.
    #[arg(long)]
    pub repo: String,
    /// Git ref to check out.
    #[arg(long = "ref")]
    pub git_ref: String,
    /// Built-in prompt template: merge, test or review.
    #[arg(long, value_parser = parse_template, default_value = "merge")]
    pub template: String,
    /// Extra instructions appended to the template prompt.
    #[arg(long = "task-file")]
    pub task_file: Option<PathBuf>,
    /// Idempotency key.
    #[arg(long)]
    pub key: Option<String>,
    /// Provider name.
    #[arg(long)]
    pub provider: Option<String>,
    /// Model name.
    #[arg(long)]
    pub model: Option<String>,
    /// Job timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Wait for the job to finish.
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, clap::Args)]
pub struct RetryArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
    /// Replace the provider name.
    #[arg(long)]
    pub provider: Option<String>,
    /// Replace the model name.
    #[arg(long)]
    pub model: Option<String>,
    /// Replace the fallback list with these provider[:model] choices (repeatable).
    #[arg(long = "fallback", value_parser = parse_fallback, conflicts_with = "no_fallbacks")]
    pub fallbacks: Vec<lu_proto::Choice>,
    /// Clear the fallback list.
    #[arg(long = "no-fallbacks")]
    pub no_fallbacks: bool,
    /// Only run after this job is done (repeatable); replaces the dependency list.
    #[arg(long = "after", value_parser = parse_job_id)]
    pub after: Vec<JobId>,
    /// Only run after the job with this idempotency key (repeatable).
    #[arg(long = "after-key")]
    pub after_key: Vec<String>,
    /// Stage dependency artifacts under .lu/inputs: none, files, apply or both.
    #[arg(long, value_name = "MODE")]
    pub collect: Option<CollectMode>,
    /// Treat failed and cancelled dependencies as inputs when they stored a diff.
    #[arg(long)]
    pub include_failed: bool,
    /// Replace the conflict policy: fail, markers or skip.
    #[arg(long = "on-conflict", value_name = "MODE")]
    pub on_conflict: Option<OnConflict>,
    /// Replace the label requirements (repeatable).
    #[arg(long = "require", conflicts_with = "no_require")]
    pub require: Vec<String>,
    /// Clear the label requirements.
    #[arg(long = "no-require")]
    pub no_require: bool,
    /// Replace the tolerated taints (repeatable).
    #[arg(long = "tolerate", conflicts_with = "no_tolerate")]
    pub tolerate: Vec<String>,
    /// Clear the tolerated taints.
    #[arg(long = "no-tolerate")]
    pub no_tolerate: bool,
    /// Idempotency key. The new job carries none unless this is given.
    #[arg(long)]
    pub key: Option<String>,
    /// Wait for the job to finish.
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, clap::Args)]
pub struct EditArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
    /// Replace the provider name; "any" lets dispatch choose a healthy pair.
    #[arg(long)]
    pub provider: Option<String>,
    /// Replace the model name. Not allowed with --provider any.
    #[arg(long)]
    pub model: Option<String>,
    /// Replace the fallbacks with these provider[:model] choices (repeatable).
    #[arg(long = "fallback", value_parser = parse_fallback, conflicts_with = "no_fallbacks")]
    pub fallbacks: Vec<lu_proto::Choice>,
    /// Clear the fallback list.
    #[arg(long = "no-fallbacks")]
    pub no_fallbacks: bool,
    /// Replace the dependencies (repeatable).
    #[arg(long = "after", value_parser = parse_job_id)]
    pub after: Vec<JobId>,
    /// Replace the dependencies with jobs named by these idempotency keys (repeatable).
    #[arg(long = "after-key")]
    pub after_key: Vec<String>,
    /// Replace the collect mode: none, files, apply or both.
    #[arg(long, value_name = "MODE")]
    pub collect: Option<CollectMode>,
    /// Include failed and cancelled dependencies as inputs when they stored a diff.
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    pub include_failed: Option<bool>,
    /// Replace the conflict policy: fail, markers or skip.
    #[arg(long = "on-conflict", value_name = "MODE")]
    pub on_conflict: Option<OnConflict>,
    /// Replace the label requirements (repeatable).
    #[arg(long = "require", conflicts_with = "no_require")]
    pub require: Vec<String>,
    /// Clear the label requirements.
    #[arg(long = "no-require")]
    pub no_require: bool,
    /// Replace the tolerated taints (repeatable).
    #[arg(long = "tolerate", conflicts_with = "no_tolerate")]
    pub tolerate: Vec<String>,
    /// Clear the tolerated taints.
    #[arg(long = "no-tolerate")]
    pub no_tolerate: bool,
}

#[derive(Debug, clap::Args)]
pub struct LsArgs {
    /// Only show jobs in this state.
    #[arg(long, value_parser = parse_state)]
    pub state: Option<JobState>,
    /// Maximum number of jobs to return.
    #[arg(long, value_parser = parse_limit)]
    pub limit: Option<u32>,
}

#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
}

#[derive(Debug, clap::Args)]
pub struct LogsArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
    /// Keep reading until the job reaches a terminal state.
    #[arg(long)]
    pub follow: bool,
}

#[derive(Debug, clap::Args)]
pub struct FetchArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
    /// Directory to write the three fixed artifact names into.
    #[arg(long, default_value = ".")]
    pub out: PathBuf,
    /// Replace files that already exist.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, clap::Args)]
pub struct CancelArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
}

#[derive(Debug, clap::Args)]
pub struct NodesArgs {}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {}

#[derive(Debug, clap::Args)]
pub struct ReposArgs {}

#[derive(Debug, clap::Args)]
pub struct SyncArgs {
    /// Repository name from the controller config.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct GraphArgs {
    /// Show only the component containing this job.
    #[arg(value_parser = parse_job_id)]
    pub job: Option<JobId>,
}

#[derive(Debug, clap::Args)]
pub struct NodeArgs {
    /// Node name.
    #[arg(value_parser = parse_node)]
    pub node: NodeName,
}

pub async fn submit(args: SubmitArgs, config: &Path, json: bool) -> ExitCode {
    finish(submit_inner(args, config, json).await)
}

pub async fn collect(args: CollectArgs, config: &Path, json: bool) -> ExitCode {
    finish(collect_inner(args, config, json).await)
}

pub async fn retry(args: RetryArgs, config: &Path, json: bool) -> ExitCode {
    finish(retry_inner(args, config, json).await)
}

pub async fn edit(args: EditArgs, config: &Path, json: bool) -> ExitCode {
    finish(edit_inner(args, config, json).await)
}

pub async fn ls(args: LsArgs, config: &Path, json: bool) -> ExitCode {
    finish(ls_inner(args, config, json).await)
}

pub async fn show(args: ShowArgs, config: &Path, json: bool) -> ExitCode {
    finish(show_inner(args, config, json).await)
}

pub async fn logs(args: LogsArgs, config: &Path, json: bool) -> ExitCode {
    finish(logs_inner(args, config, json).await)
}

pub async fn fetch(args: FetchArgs, config: &Path, json: bool) -> ExitCode {
    finish(fetch_inner(args, config, json).await)
}

pub async fn cancel(args: CancelArgs, config: &Path, json: bool) -> ExitCode {
    finish(cancel_inner(args, config, json).await)
}

pub async fn nodes(args: NodesArgs, config: &Path, json: bool) -> ExitCode {
    finish(nodes_inner(args, config, json).await)
}

pub async fn status(args: StatusArgs, config: &Path, json: bool) -> ExitCode {
    finish(status_inner(args, config, json).await)
}

pub async fn repos(args: ReposArgs, config: &Path, json: bool) -> ExitCode {
    finish(repos_inner(args, config, json).await)
}

pub async fn sync_repo(args: SyncArgs, config: &Path, json: bool) -> ExitCode {
    finish(sync_repo_inner(args, config, json).await)
}

pub async fn node(args: NodeArgs, config: &Path, json: bool) -> ExitCode {
    finish(node_inner(args, config, json).await)
}

pub async fn graph(args: GraphArgs, config: &Path, json: bool) -> ExitCode {
    finish(graph_inner(args, config, json).await)
}

pub async fn drain(args: NodeArgs, config: &Path, json: bool) -> ExitCode {
    finish(drain_inner(args, config, json, true).await)
}

pub async fn undrain(args: NodeArgs, config: &Path, json: bool) -> ExitCode {
    finish(drain_inner(args, config, json, false).await)
}

async fn submit_inner(args: SubmitArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let task = read_task(&args)?;
    let client = connect(config)?;
    let Some(after) = resolve_after(&client, &args.after, &args.after_key).await? else {
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    let spec = JobSpec {
        repo: args.repo,
        git_ref: args.git_ref,
        base_commit: None,
        task,
        harness: args.harness,
        provider: args.provider,
        model: args.model,
        timeout_secs: args.timeout,
        idle_timeout_secs: args.idle_timeout,
        idempotency_key: args.key,
        after,
        stack_dependencies: !args.no_stack,
        collect: args.collect,
        include_failed: args.include_failed,
        on_conflict: args.on_conflict,
        allow_empty: args.allow_empty,
        requires: args.require,
        tolerates: args.tolerate,
        fallbacks: args.fallbacks,
    };
    spec.validate().map_err(|error| error.to_string())?;

    submit_and_report(&client, &spec, json, args.wait).await
}

async fn collect_inner(args: CollectArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let mut task = crate::templates::prompt(&args.template)?;
    if let Some(path) = &args.task_file {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let extra = read_bounded(file, MAX_TASK_BYTES)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let base = task.trim_end().to_owned();
        task = format!("{base}\n\n## Extra instructions\n\n{extra}");
    }
    if task.len() > MAX_TASK_BYTES {
        eprintln!("lu: collected prompt is larger than {MAX_TASK_BYTES} bytes");
        return Ok(ExitCode::from(EXIT_USAGE));
    }

    let client = connect(config)?;
    let mut after = Vec::with_capacity(args.inputs.len());
    let mut include_failed = false;
    for input in &args.inputs {
        let view = if let Ok(id) = JobId::parse(input) {
            match client.get_job(&id).await {
                Ok(view) => view,
                Err(ApiError::NotFound) => {
                    eprintln!("lu: no job with id {input:?}");
                    return Ok(ExitCode::from(EXIT_USAGE));
                }
                Err(error) => return Err(error.to_string()),
            }
        } else {
            match client
                .job_by_key(input)
                .await
                .map_err(|error| error.to_string())?
            {
                Some(view) => view,
                None => {
                    eprintln!("lu: no job with idempotency key {input:?}");
                    return Ok(ExitCode::from(EXIT_USAGE));
                }
            }
        };
        if view.state == JobState::Failed && view.diff_bytes.unwrap_or(0) > 0 {
            include_failed = true;
        }
        if !after.contains(&view.id) {
            after.push(view.id);
        }
    }

    let spec = JobSpec {
        repo: args.repo,
        git_ref: args.git_ref,
        base_commit: None,
        task,
        harness: "opencode".to_owned(),
        provider: args.provider,
        model: args.model,
        timeout_secs: args.timeout,
        idle_timeout_secs: None,
        idempotency_key: args.key,
        after,
        stack_dependencies: true,
        collect: Some(CollectMode::Both),
        include_failed,
        on_conflict: OnConflict::Markers,
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        fallbacks: Vec::new(),
    };
    spec.validate().map_err(|error| error.to_string())?;
    submit_and_report(&client, &spec, json, args.wait).await
}

/// Turns `--after-key` values into ids and merges them with explicit `--after` ids, dropping
/// repeats. `None` means a key was unknown and the usage error has already been printed.
async fn resolve_after(
    client: &Client,
    after: &[JobId],
    keys: &[String],
) -> Result<Option<Vec<JobId>>, String> {
    let mut ids = after.to_vec();
    for key in keys {
        let view = match client.job_by_key(key).await.map_err(|e| e.to_string())? {
            Some(view) => view,
            None => {
                eprintln!("lu: no job with idempotency key {key:?}");
                return Ok(None);
            }
        };
        if !ids.contains(&view.id) {
            ids.push(view.id);
        }
    }
    Ok(Some(ids))
}

async fn retry_inner(args: RetryArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let view = client
        .get_job(&args.id)
        .await
        .map_err(|error| error.to_string())?;
    if !view.state.is_terminal() {
        eprintln!(
            "lu: job {} is {}; retry resubmits a finished job (done, failed or cancelled)",
            view.id,
            state_name(view.state)
        );
        return Ok(ExitCode::from(EXIT_USAGE));
    }

    let mut spec = view.spec;
    if let Some(provider) = args.provider {
        spec.provider = Some(provider);
    }
    if let Some(model) = args.model {
        spec.model = Some(model);
    }
    if !args.after.is_empty() || !args.after_key.is_empty() {
        let Some(after) = resolve_after(&client, &args.after, &args.after_key).await? else {
            return Ok(ExitCode::from(EXIT_USAGE));
        };
        spec.after = after;
    }
    if !args.require.is_empty() {
        spec.requires = args.require;
    } else if args.no_require {
        spec.requires.clear();
    }
    if !args.tolerate.is_empty() {
        spec.tolerates = args.tolerate;
    } else if args.no_tolerate {
        spec.tolerates.clear();
    }
    if !args.fallbacks.is_empty() {
        spec.fallbacks = args.fallbacks;
    } else if args.no_fallbacks {
        spec.fallbacks.clear();
    }
    if let Some(collect) = args.collect {
        spec.collect = Some(collect);
    }
    if args.include_failed {
        spec.include_failed = true;
    }
    if let Some(on_conflict) = args.on_conflict {
        spec.on_conflict = on_conflict;
    }
    // A retry is a new job; only --key gives it an idempotency key.
    spec.idempotency_key = args.key;
    spec.validate().map_err(|error| error.to_string())?;

    submit_and_report(&client, &spec, json, args.wait).await
}

async fn edit_inner(args: EditArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    if args.provider.as_deref() == Some(lu_proto::ANY_PROVIDER)
        && (args.model.is_some() || !args.fallbacks.is_empty())
    {
        return Err("lu: --provider any cannot be combined with --model or --fallback".into());
    }

    let client = connect(config)?;
    let after = if args.after.is_empty() && args.after_key.is_empty() {
        None
    } else {
        match resolve_after(&client, &args.after, &args.after_key).await? {
            Some(ids) => Some(ids),
            None => return Ok(ExitCode::from(EXIT_USAGE)),
        }
    };
    let edit = JobEdit {
        provider: args.provider,
        model: args.model,
        fallbacks: (!args.fallbacks.is_empty() || args.no_fallbacks).then_some(args.fallbacks),
        requires: (!args.require.is_empty() || args.no_require).then_some(args.require),
        tolerates: (!args.tolerate.is_empty() || args.no_tolerate).then_some(args.tolerate),
        after,
        collect: args.collect,
        include_failed: args.include_failed,
        on_conflict: args.on_conflict,
    };
    let (view, raw) = client
        .edit_with_raw(&args.id, &edit)
        .await
        .map_err(|error| error.to_string())?;
    if json {
        print_raw(&raw);
    } else {
        println!("{} {}", view.id, state_name(view.state));
    }
    Ok(ExitCode::SUCCESS)
}

async fn submit_and_report(
    client: &Client,
    spec: &JobSpec,
    json: bool,
    wait: bool,
) -> Result<ExitCode, String> {
    let (view, raw) = client
        .submit_with_raw(spec)
        .await
        .map_err(|error| error.to_string())?;

    for warning in view.warnings() {
        eprintln!("lu: warning: {warning}");
    }

    if json && !wait {
        print_raw(&raw);
    } else {
        println!("{}", view.id);
    }

    if !wait {
        return Ok(ExitCode::SUCCESS);
    }

    let final_view = wait_for_job(client, &view.id).await?;
    eprintln!("state: {}", state_name(final_view.state));
    if json {
        let raw = client
            .get_job_raw(&view.id)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
    }
    Ok(if final_view.state == JobState::Done {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_UNSUCCESSFUL)
    })
}

async fn ls_inner(args: LsArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .list_jobs_raw(args.state, args.limit)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }

    let page = client
        .list_jobs(args.state, args.limit)
        .await
        .map_err(|error| error.to_string())?;
    let rows = page
        .jobs
        .iter()
        .map(|job| {
            vec![
                job.id.to_string(),
                state_name(job.state).to_owned(),
                job.node
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "-".to_owned()),
                job.attempt.to_string(),
                if job.waiting_on.is_empty() {
                    "-".to_owned()
                } else {
                    job.waiting_on.len().to_string()
                },
                time(job.created_at),
                outcome_text(job.outcome_note.as_deref()),
            ]
        })
        .collect();
    print_table(
        &["ID", "STATE", "NODE", "ATTEMPT", "AFTER", "CREATED", "NOTE"],
        rows,
    );
    Ok(ExitCode::SUCCESS)
}

async fn show_inner(args: ShowArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .get_job_raw(&args.id)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let view = client
        .get_job(&args.id)
        .await
        .map_err(|error| error.to_string())?;
    print_job(&view);
    for id in &view.after {
        let state = match client.get_job(id).await {
            Ok(dep) => state_name(dep.state).to_owned(),
            Err(_) => "unknown".to_owned(),
        };
        let waiting = if view.waiting_on.contains(id) {
            " (waiting)"
        } else {
            ""
        };
        println!("after  {id}  {state}{waiting}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn logs_inner(args: LogsArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if !args.follow {
        if json {
            let raw = client
                .logs_raw(&args.id, 0, None)
                .await
                .map_err(|error| error.to_string())?;
            print_raw(&raw);
        } else {
            let page = client
                .logs(&args.id, 0, None)
                .await
                .map_err(|error| error.to_string())?;
            print_log(&page.data);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut offset = 0u64;
    loop {
        let (page, raw) = client
            .logs_with_raw(&args.id, offset, Some(FOLLOW_WAIT_SECS))
            .await
            .map_err(|error| error.to_string())?;
        if json {
            print_raw(&raw);
        } else {
            print_log(&page.data);
        }
        let previous = offset;
        offset = offset.max(page.offset);
        if page.terminal || page.state.is_some_and(JobState::is_terminal) {
            return Ok(follow_exit(&page));
        }
        if page.data.is_empty() && offset == previous {
            // A controller that answers an empty wait immediately must not make us spin.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

fn follow_exit(page: &LogPage) -> ExitCode {
    match page.state {
        Some(JobState::Failed | JobState::Cancelled) => ExitCode::from(EXIT_UNSUCCESSFUL),
        _ => ExitCode::SUCCESS,
    }
}

async fn fetch_inner(args: FetchArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let mut saved = Vec::new();
    let mut skipped = Vec::new();
    for name in ArtifactName::ALL {
        match client.fetch_artifact(&args.id, name).await {
            Ok(bytes) => {
                let path = write_artifact(&args.out, name, &bytes, args.force)?;
                if !json {
                    println!("saved {}", path.display());
                }
                saved.push(path.display().to_string());
            }
            Err(ApiError::NotFound) => {
                if !json {
                    eprintln!("skipped {}: not found", name.file_name());
                }
                skipped.push(name.file_name().to_owned());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    if saved.is_empty() {
        return Err("no artifacts were available".to_owned());
    }
    if json {
        let summary = serde_json::json!({ "saved": saved, "skipped": skipped });
        println!("{summary}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn cancel_inner(args: CancelArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .cancel_raw(&args.id)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let view = client
        .cancel(&args.id)
        .await
        .map_err(|error| error.to_string())?;
    print_job(&view);
    Ok(ExitCode::SUCCESS)
}

async fn nodes_inner(_args: NodesArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .nodes_raw()
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let list = client.nodes().await.map_err(|error| error.to_string())?;
    let rows = list
        .nodes
        .iter()
        .map(|node| {
            // Taints first: they decide dispatch eligibility, so keep them visible when labels overflow.
            let mut labels: Vec<String> = node
                .taints
                .iter()
                .map(|taint| format!("!{taint}"))
                .collect();
            labels.extend(node.labels.iter().map(|label| {
                if node.label_sources.get(label).map(String::as_str) == Some(LABEL_SOURCE_PROBED) {
                    format!("{label} (probed)")
                } else {
                    label.clone()
                }
            }));
            vec![
                node.name.to_string(),
                node.version
                    .as_deref()
                    .map(display_field)
                    .unwrap_or_else(|| "-".to_owned()),
                harness_name(node.harness),
                labels_cell(&labels, 40),
                time(node.last_seen),
                if node.draining { "yes" } else { "no" }.to_owned(),
            ]
        })
        .collect();
    print_table(
        &[
            "NAME",
            "VERSION",
            "HARNESS",
            "LABELS",
            "LAST_SEEN",
            "DRAINED",
        ],
        rows,
    );
    Ok(ExitCode::SUCCESS)
}

async fn status_inner(_args: StatusArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .status_raw()
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let status = client.status().await.map_err(|error| error.to_string())?;
    print_status(&status);
    Ok(ExitCode::SUCCESS)
}

async fn repos_inner(_args: ReposArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .repos_raw()
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let list = client.repos().await.map_err(|error| error.to_string())?;
    print_repos(&list);
    Ok(ExitCode::SUCCESS)
}

async fn sync_repo_inner(args: SyncArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .sync_repo_raw(&args.name)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let repo = client
        .sync_repo(&args.name)
        .await
        .map_err(|error| error.to_string())?;
    print_repo(&repo);
    Ok(ExitCode::SUCCESS)
}

async fn node_inner(args: NodeArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    if json {
        let raw = client
            .node_stats_raw(&args.node)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let stats = client
        .node_stats(&args.node)
        .await
        .map_err(|error| error.to_string())?;
    print_node_stats(&args.node, &stats);
    Ok(ExitCode::SUCCESS)
}

const GRAPH_WIDTH: usize = 120;

async fn graph_inner(args: GraphArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let mut views = client
        .list_jobs(None, Some(200))
        .await
        .map_err(|error| error.to_string())?
        .jobs;
    if let Some(id) = &args.job {
        if !views.iter().any(|view| &view.id == id) {
            let target = client
                .get_job(id)
                .await
                .map_err(|error| error.to_string())?;
            views.push(target);
        }
    }
    let jobs: Vec<graph::Job> = views.iter().map(graph_job).collect();
    let graph = match &args.job {
        Some(id) => Graph::for_job(&jobs, id.as_str())
            .ok_or_else(|| format!("job {id} is not in the job list"))?,
        None => Graph::build(&jobs),
    };
    if json {
        print_graph_json(&graph);
        return Ok(ExitCode::SUCCESS);
    }
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(GRAPH_WIDTH);
    let colour = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    for line in graph::render(&graph, width) {
        if colour {
            print_graph_line(&line);
        } else {
            println!("{}", line.text());
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn graph_job(view: &JobView) -> graph::Job {
    graph::Job {
        id: view.id.to_string(),
        state: view.state,
        title: job_title(&view.spec.task),
        after: view.after.iter().map(ToString::to_string).collect(),
    }
}

fn print_graph_json(graph: &Graph) {
    let nodes: Vec<serde_json::Value> = graph
        .components
        .iter()
        .flat_map(|component| &component.nodes)
        .map(|node| {
            serde_json::json!({
                "id": node.id,
                "state": node.state.as_str(),
                "title": node.title,
                "after": node.after,
            })
        })
        .collect();
    let body = serde_json::json!({ "nodes": nodes });
    match serde_json::to_string(&body) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("lu: cannot encode the graph: {error}"),
    }
}

fn print_graph_line(line: &graph::GraphLine) {
    let mut out = String::new();
    for segment in &line.segments {
        match ansi(segment.tone) {
            Some(code) => {
                out.push_str(code);
                out.push_str(&segment.text);
                out.push_str("\u{1b}[0m");
            }
            None => out.push_str(&segment.text),
        }
    }
    println!("{out}");
}

fn ansi(tone: Tone) -> Option<&'static str> {
    match tone {
        Tone::Plain | Tone::Heading => None,
        Tone::Good => Some("\u{1b}[1;92m"),
        Tone::Warn => Some("\u{1b}[33m"),
        Tone::Bad => Some("\u{1b}[31m"),
        Tone::Dim => Some("\u{1b}[90m"),
        Tone::Done => Some("\u{1b}[2;32m"),
        Tone::Blocked => Some("\u{1b}[38;5;208m"),
    }
}

async fn drain_inner(
    args: NodeArgs,
    config: &Path,
    json: bool,
    drain: bool,
) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let result = if json {
        client
            .set_drain_raw(&args.node, drain)
            .await
            .map(|raw| print_raw(&raw))
    } else {
        client
            .set_drain(&args.node, drain)
            .await
            .map(|view| print_node(&view))
    };
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(ApiError::NotFound) => Err(format!(
            "node '{}' not found: a node must heartbeat at least once before it can be drained or undrained",
            args.node.as_str()
        )),
        Err(error) => Err(error.to_string()),
    }
}

async fn wait_for_job(client: &Client, id: &JobId) -> Result<JobView, String> {
    loop {
        let view = client
            .get_job(id)
            .await
            .map_err(|error| error.to_string())?;
        if view.state.is_terminal() {
            return Ok(view);
        }
        tokio::time::sleep(WAIT_POLL_INTERVAL).await;
    }
}

struct Settings {
    url: String,
    token_file: PathBuf,
    ca_file: Option<PathBuf>,
}

pub(crate) fn connect(config: &Path) -> Result<Client, String> {
    let settings = settings(config)?;
    let text = lu_config::read_private_file("operator token", &settings.token_file, TOKEN_LIMIT)
        .map_err(|error| error.to_string())?;
    let token = text.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        return Err(format!(
            "token file {} is empty",
            settings.token_file.display()
        ));
    }
    Client::new(&settings.url, token, settings.ca_file).map_err(|error| error.to_string())
}

fn settings(_config: &Path) -> Result<Settings, String> {
    if let Ok(url) = std::env::var("LU_URL") {
        if !url.is_empty() {
            let token_file = std::env::var("LU_TOKEN_FILE")
                .ok()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| "LU_TOKEN_FILE must be set when LU_URL is set".to_owned())?;
            let ca_file = std::env::var("LU_CA_FILE")
                .ok()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            return Ok(Settings {
                url,
                token_file: PathBuf::from(token_file),
                ca_file,
            });
        }
    }

    let found = lu_config::operator_config_paths()
        .into_iter()
        .find(|path| path.exists());
    if let Some(path) = found {
        let file = lu_config::load_operator(&path).map_err(|error| error.to_string())?;
        return Ok(Settings {
            url: file.url,
            token_file: PathBuf::from(file.token_file),
            ca_file: file
                .ca_file
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        });
    }
    Err("no controller URL: set LU_URL and LU_TOKEN_FILE, or write ~/.config/lu/operator.toml or /etc/lu/operator.toml".to_owned())
}

fn read_task(args: &SubmitArgs) -> Result<String, String> {
    if let Some(task) = &args.task {
        return Ok(task.clone());
    }
    if let Some(path) = &args.task_file {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        return read_bounded(file, MAX_TASK_BYTES)
            .map_err(|error| format!("cannot read {}: {error}", path.display()));
    }
    read_bounded(std::io::stdin().lock(), MAX_TASK_BYTES)
        .map_err(|error| format!("cannot read the task from stdin: {error}"))
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > limit {
        return Err(format!("task is larger than {limit} bytes"));
    }
    String::from_utf8(bytes).map_err(|_| "task is not valid UTF-8".to_owned())
}

fn write_artifact(
    dir: &Path,
    name: ArtifactName,
    bytes: &[u8],
    force: bool,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    // Only the three fixed names can reach this join; the job id is never a path component.
    let path = dir.join(name.file_name());
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} already exists (use --force)", path.display())
        } else {
            format!("cannot write {}: {error}", path.display())
        }
    })?;
    std::io::Write::write_all(&mut file, bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot set the mode of {}: {error}", path.display()))?;
    }
    Ok(path)
}

fn print_job(view: &JobView) {
    let fallbacks = view
        .spec
        .fallbacks
        .iter()
        .map(|choice| match &choice.model {
            Some(model) => format!(
                "{}:{}",
                display_field(&choice.provider),
                display_field(model)
            ),
            None => display_field(&choice.provider),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let rows = vec![
        ("id", view.id.to_string()),
        ("state", state_name(view.state).to_owned()),
        (
            "node",
            view.node
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "assigned_provider",
            view.assigned_provider
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "fallbacks",
            if fallbacks.is_empty() {
                "-".to_owned()
            } else {
                fallbacks
            },
        ),
        (
            "tolerates",
            if view.spec.tolerates.is_empty() {
                "-".to_owned()
            } else {
                view.spec
                    .tolerates
                    .iter()
                    .map(|taint| display_field(taint))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
        ("attempt", view.attempt.to_string()),
        ("epoch", view.epoch.to_string()),
        ("created_at", time(view.created_at)),
        ("started_at", optional_time(view.started_at)),
        ("finished_at", optional_time(view.finished_at)),
        ("failure", failure_name(view.failure_class)),
        (
            "waiting",
            view.waiting_reason
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "summary",
            view.summary
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        ("outcome_note", outcome_text(view.outcome_note.as_deref())),
        (
            "diff_bytes",
            view.diff_bytes
                .map(|bytes| bytes.to_string())
                .unwrap_or_else(|| "-".to_owned()),
        ),
    ];
    print_pairs(&rows);
}

fn print_status(status: &StatusView) {
    print_pairs(&[
        ("version", status.version.clone()),
        ("uptime_secs", status.uptime_secs.to_string()),
        ("schema_version", status.schema_version.to_string()),
        ("db_bytes", status.db_bytes.to_string()),
        ("recovered_on_boot", status.recovered_on_boot.to_string()),
        ("oldest_queued_secs", status.oldest_queued_secs.to_string()),
        ("nodes", status.nodes.to_string()),
        ("nodes_online", status.nodes_online.to_string()),
        ("lease_secs", status.lease_secs.to_string()),
    ]);
    println!();
    let rows: Vec<Vec<String>> = JobState::ALL
        .iter()
        .map(|state| {
            vec![
                state_name(*state).to_owned(),
                status
                    .jobs
                    .get(state_name(*state))
                    .copied()
                    .unwrap_or(0)
                    .to_string(),
            ]
        })
        .collect();
    print_table(&["STATE", "COUNT"], rows);
}

fn print_repos(list: &ListRepos) {
    let rows = list
        .repos
        .iter()
        .map(|repo| {
            vec![
                display_field(&repo.name),
                display_field(&repo.url),
                optional_time(repo.synced_at),
                repo.heads.len().to_string(),
            ]
        })
        .collect();
    print_table(&["NAME", "URL", "SYNCED", "HEADS"], rows);
}

fn print_repo(repo: &RepoView) {
    print_pairs(&[
        ("name", display_field(&repo.name)),
        ("synced_at", optional_time(repo.synced_at)),
    ]);
    if repo.heads.is_empty() {
        println!("heads  -");
        return;
    }
    let rows = repo
        .heads
        .iter()
        .map(|(branch, commit)| vec![display_field(branch), commit.clone()])
        .collect();
    print_indented_table(&["BRANCH", "COMMIT"], rows);
}

fn print_node(node: &NodeView) {
    let rows = vec![
        ("name", node.name.to_string()),
        (
            "version",
            node.version
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        ("harness", harness_name(node.harness)),
        ("last_seen", time(node.last_seen)),
        (
            "draining",
            if node.draining { "yes" } else { "no" }.to_owned(),
        ),
    ];
    print_pairs(&rows);
}

fn print_node_stats(node: &NodeName, stats: &NodeStats) {
    println!("node  {node}");
    let labels: Vec<String> = stats
        .labels
        .iter()
        .map(|label| {
            let shown = printable(label);
            if stats.label_sources.get(label).map(String::as_str) == Some(LABEL_SOURCE_PROBED) {
                format!("{shown} (probed)")
            } else {
                shown
            }
        })
        .collect();
    println!(
        "labels {}",
        if labels.is_empty() {
            "-".to_owned()
        } else {
            labels.join(" ")
        }
    );
    match &stats.host {
        Some(host) => print_pairs(&[
            ("load1", format!("{:.2}", host.load1)),
            ("load5", format!("{:.2}", host.load5)),
            ("load15", format!("{:.2}", host.load15)),
            ("mem_total_kb", host.mem_total_kb.to_string()),
            ("mem_available_kb", host.mem_available_kb.to_string()),
            ("swap_used_kb", host.swap_used_kb.to_string()),
            (
                "disk_free_bytes",
                host.disk_free_bytes
                    .map(|bytes| bytes.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            (
                "disk_total_bytes",
                host.disk_total_bytes
                    .map(|bytes| bytes.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            (
                "disk_free_percent",
                disk_free_percent(host.disk_free_bytes, host.disk_total_bytes)
                    .map(|percent| percent.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            ("uptime_secs", host.uptime_secs.to_string()),
        ]),
        None => println!("host  -"),
    }
    if stats.jobs.is_empty() {
        println!("jobs  -");
        return;
    }
    for job in &stats.jobs {
        println!();
        println!(
            "job {}  pgid {}  output_idle {}s  cpu_idle {}s  stuck {}",
            job.job_id,
            job.pgid,
            job.output_idle_secs,
            job.cpu_idle_secs,
            if job.stuck { "yes" } else { "no" }
        );
        let rows = job
            .processes
            .iter()
            .map(|process| {
                vec![
                    process.pid.to_string(),
                    process.ppid.to_string(),
                    display_field(&process.state),
                    process.cpu_secs.to_string(),
                    process.rss_kb.to_string(),
                    process.age_secs.to_string(),
                    process.open_files.to_string(),
                    display_field(&process.cmdline),
                ]
            })
            .collect();
        print_indented_table(
            &[
                "PID", "PPID", "STATE", "CPU_S", "RSS_KB", "AGE_S", "FILES", "COMMAND",
            ],
            rows,
        );
    }
}

fn disk_free_percent(free: Option<u64>, total: Option<u64>) -> Option<u64> {
    let (free, total) = (free?, total?);
    free.saturating_mul(100).checked_div(total)
}

fn print_pairs(rows: &[(&str, String)]) {
    let width = rows.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    for (key, value) in rows {
        println!("{key:<width$}  {value}");
    }
}

fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    print_row(headers, &widths);
    for row in &rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        print_row(&cells, &widths);
    }
}

fn print_row(cells: &[&str], widths: &[usize]) {
    println!("{}", row_line(cells, widths));
}

fn print_indented_table(headers: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    println!("  {}", row_line(headers, &widths));
    for row in &rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        println!("  {}", row_line(&cells, &widths));
    }
}

fn row_line(cells: &[&str], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        let width = widths.get(index).copied().unwrap_or(0);
        if index + 1 == cells.len() {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{cell:<width$}"));
        }
    }
    line
}

fn print_log(data: &str) {
    if data.is_empty() {
        return;
    }
    // Keep line breaks (they are the shape of a log) but sanitise every character inside a line.
    for piece in data.split_inclusive('\n') {
        match piece.strip_suffix('\n') {
            Some(line) => println!("{}", printable(line)),
            None => print!("{}", printable(piece)),
        }
    }
}

fn print_raw(raw: &str) {
    if raw.is_empty() {
        return;
    }
    print!("{raw}");
    if !raw.ends_with('\n') {
        println!();
    }
}

fn display_field(text: &str) -> String {
    printable(&truncate_chars(text, SUMMARY_CHARS))
}

/// Joins labels for a table cell, dropping the tail and appending `+N` when the column is full.
fn labels_cell(labels: &[String], width: usize) -> String {
    if labels.is_empty() {
        return "-".to_owned();
    }
    let mut cell = String::new();
    let mut shown = 0;
    for (index, label) in labels.iter().enumerate() {
        let suffix = labels.len() - index - 1;
        let suffix_len = if suffix > 0 {
            format!("+{suffix}").chars().count()
        } else {
            0
        };
        let separator = usize::from(shown > 0);
        let needed = separator + label.chars().count() + suffix_len;
        if index == 0 {
            if needed > width && width > suffix_len {
                cell.push_str(&truncate_chars(label, width - suffix_len));
                shown = 1;
                break;
            }
        } else if cell.chars().count() + needed > width {
            break;
        }
        if shown > 0 {
            cell.push(',');
        }
        cell.push_str(label);
        shown = index + 1;
    }
    let remaining = labels.len() - shown;
    if remaining > 0 {
        cell.push('+');
        cell.push_str(&remaining.to_string());
    }
    cell
}

/// Unix seconds as `YYYY-MM-DD HH:MM:SSZ`, computed by hand to avoid a date dependency.
fn time(value: u64) -> String {
    if value == 0 {
        return "-".to_owned();
    }
    let days = (value / 86_400) as i64;
    let secs = value % 86_400;
    // Civil-from-days (proleptic Gregorian), valid far beyond any real timestamp.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn optional_time(value: Option<u64>) -> String {
    value.map(time).unwrap_or_else(|| "-".to_owned())
}

/// The job's title: the first non-empty line of its task, without heading marks or a `Task:` lead.
pub(crate) fn job_title(task: &str) -> String {
    let line = task
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = line
        .strip_prefix("Task:")
        .or_else(|| line.strip_prefix("task:"))
        .unwrap_or(line)
        .trim();
    line.chars().filter(|c| !c.is_control()).collect()
}

pub(crate) fn harness_name(value: Option<HarnessState>) -> String {
    value
        .map(|state| state.as_str().to_owned())
        .unwrap_or_else(|| "-".to_owned())
}

pub(crate) fn failure_name(value: Option<FailureClass>) -> String {
    value
        .map(|class| class.as_str().to_owned())
        .unwrap_or_else(|| "-".to_owned())
}

/// The controller sends a small fixed set of notes; only display the one we understand.
pub(crate) fn outcome_text(note: Option<&str>) -> String {
    match note {
        Some("no_changes") => "no changes".to_owned(),
        _ => "-".to_owned(),
    }
}

pub(crate) fn state_name(state: JobState) -> &'static str {
    state.as_str()
}

fn parse_job_id(value: &str) -> Result<JobId, String> {
    JobId::parse(value).map_err(|error| error.to_string())
}

fn parse_template(value: &str) -> Result<String, String> {
    if crate::templates::builtin(value).is_some() {
        return Ok(value.to_owned());
    }
    Err(format!(
        "unknown template {value:?}; expected {}",
        crate::templates::NAMES.join(", ")
    ))
}

fn parse_node(value: &str) -> Result<NodeName, String> {
    NodeName::parse(value).map_err(|error| error.to_string())
}

/// `provider` or `provider:model`; the model may contain colons, the provider cannot.
fn parse_fallback(value: &str) -> Result<lu_proto::Choice, String> {
    let (provider, model) = match value.split_once(':') {
        Some((provider, model)) => (provider, Some(model.to_owned())),
        None => (value, None),
    };
    Ok(lu_proto::Choice {
        provider: provider.to_owned(),
        model,
    })
}

fn parse_state(value: &str) -> Result<JobState, String> {
    let normalized = value.to_ascii_lowercase().replace('-', "_");
    // CLI-only spelling; the controller's wire state is always `cancelled`.
    if normalized == "canceled" {
        return Ok(JobState::Cancelled);
    }
    normalized.parse().map_err(|_| {
        format!(
            "unknown job state {normalized:?}; expected blocked, queued, leased, running, recovering, needs_input, done, failed, or cancelled"
        )
    })
}

fn parse_idle_timeout(value: &str) -> Result<u64, String> {
    let secs: u64 = value
        .parse()
        .map_err(|_| format!("idle timeout must be a whole number of seconds (got {value:?})"))?;
    let max = lu_proto::MAX_IDLE_TIMEOUT_SECS;
    if !(lu_proto::MIN_TIMEOUT_SECS..=max).contains(&secs) {
        return Err(format!(
            "idle timeout must be {} to {max} seconds (got {secs})",
            lu_proto::MIN_TIMEOUT_SECS
        ));
    }
    Ok(secs)
}

fn parse_limit(value: &str) -> Result<u32, String> {
    let limit: u32 = value
        .parse()
        .map_err(|_| format!("limit must be a positive integer (got {value:?})"))?;
    if limit == 0 {
        return Err("limit must be at least 1".to_owned());
    }
    Ok(limit)
}

fn finish(result: Result<ExitCode, String>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("lu: {message}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

#[cfg(test)]
mod label_tests {
    use super::labels_cell;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn labels_render_comma_separated_and_cut_with_a_count() {
        assert_eq!(labels_cell(&[], 40), "-");
        assert_eq!(labels_cell(&labels(&["php8.3", "gpu"]), 40), "php8.3,gpu");
        assert_eq!(
            labels_cell(&labels(&["php8.3", "gpu", "region=eu"]), 13),
            "php8.3,gpu+1"
        );
        assert_eq!(labels_cell(&labels(&["a-very-long-label"]), 8), "a-very-l");
    }
}

#[cfg(test)]
mod time_tests {
    use super::time;

    #[test]
    fn unix_seconds_render_as_utc() {
        assert_eq!(time(0), "-");
        assert_eq!(time(1_794_818_368), "2026-11-16 08:39:28Z");
        assert_eq!(time(951_782_400), "2000-02-29 00:00:00Z");
    }
}

#[cfg(test)]
mod collect_cli_tests {
    use clap::Parser;
    use lu_proto::{CollectMode, OnConflict};

    use super::{CollectArgs, EditArgs, RetryArgs, SubmitArgs};

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Debug, clap::Subcommand)]
    enum Command {
        Submit(SubmitArgs),
        Collect(CollectArgs),
        Retry(RetryArgs),
        Edit(EditArgs),
    }

    #[test]
    fn submit_parses_collect_and_include_failed() {
        let cli = Cli::try_parse_from([
            "lu",
            "submit",
            "--repo",
            "https://example.net/r.git",
            "--ref",
            "main",
            "--task",
            "go",
            "--collect",
            "files",
            "--include-failed",
            "--on-conflict",
            "markers",
        ])
        .unwrap();
        let Command::Submit(args) = cli.command else {
            panic!("expected submit");
        };
        assert_eq!(args.collect, Some(CollectMode::Files));
        assert!(args.include_failed);
        assert_eq!(args.on_conflict, OnConflict::Markers);

        let cli = Cli::try_parse_from([
            "lu",
            "submit",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--task",
            "go",
        ])
        .unwrap();
        let Command::Submit(args) = cli.command else {
            panic!("expected submit");
        };
        assert_eq!(args.collect, None);
        assert!(!args.include_failed);
        assert_eq!(args.on_conflict, OnConflict::Fail);
    }

    #[test]
    fn retry_and_edit_parse_collect_and_include_failed() {
        let id = lu_proto::JobId::new().to_string();
        let cli = Cli::try_parse_from([
            "lu",
            "retry",
            &id,
            "--collect",
            "patch",
            "--include-failed",
            "--on-conflict",
            "skip",
        ])
        .unwrap();
        let Command::Retry(args) = cli.command else {
            panic!("expected retry");
        };
        assert_eq!(args.collect, Some(CollectMode::Files));
        assert!(args.include_failed);
        assert_eq!(args.on_conflict, Some(OnConflict::Skip));

        let cli = Cli::try_parse_from([
            "lu",
            "edit",
            &id,
            "--collect",
            "both",
            "--include-failed",
            "false",
            "--on-conflict",
            "fail",
        ])
        .unwrap();
        let Command::Edit(args) = cli.command else {
            panic!("expected edit");
        };
        assert_eq!(args.collect, Some(CollectMode::Both));
        assert_eq!(args.include_failed, Some(false));
        assert_eq!(args.on_conflict, Some(OnConflict::Fail));

        let cli = Cli::try_parse_from(["lu", "edit", &id, "--include-failed"]).unwrap();
        let Command::Edit(args) = cli.command else {
            panic!("expected edit");
        };
        assert_eq!(args.include_failed, Some(true));
    }

    #[test]
    fn collect_parses_inputs_template_and_options() {
        let cli = Cli::try_parse_from([
            "lu",
            "collect",
            "build",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--template",
            "review",
            "--task-file",
            "/tmp/extra.md",
            "--key",
            "combined",
            "--provider",
            "p",
            "--model",
            "m",
            "--timeout",
            "90",
            "--wait",
        ])
        .unwrap();
        let Command::Collect(args) = cli.command else {
            panic!("expected collect");
        };
        assert_eq!(args.inputs, ["build", "01ARZ3NDEKTSV4RRFFQ69G5FAV"]);
        assert_eq!(args.template, "review");
        assert_eq!(
            args.task_file,
            Some(std::path::PathBuf::from("/tmp/extra.md"))
        );
        assert_eq!(args.key.as_deref(), Some("combined"));
        assert_eq!(args.provider.as_deref(), Some("p"));
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(args.timeout, 90);
        assert!(args.wait);

        let cli = Cli::try_parse_from([
            "lu",
            "collect",
            "build",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
        ])
        .unwrap();
        let Command::Collect(args) = cli.command else {
            panic!("expected collect");
        };
        assert_eq!(args.template, "merge");
        assert!(!args.wait);
    }

    #[test]
    fn an_unknown_template_is_a_clap_error() {
        let error = Cli::try_parse_from([
            "lu",
            "collect",
            "build",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--template",
            "nope",
        ])
        .unwrap_err();
        assert!(error.to_string().contains("template"), "{error}");
    }

    #[test]
    fn an_invalid_collect_mode_is_a_clap_error() {
        let error = Cli::try_parse_from([
            "lu",
            "submit",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--task",
            "go",
            "--collect",
            "nope",
        ])
        .unwrap_err();
        assert!(error.to_string().contains("collect"), "{error}");
    }
}
