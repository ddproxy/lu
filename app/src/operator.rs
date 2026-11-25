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
    printable, truncate_chars, ArtifactName, CollectMode, ExpectEdit, ExpectMode, ExpectSpec,
    FailureClass, HarnessState, JobEdit, JobId, JobSpec, JobState, NodeName, OnConflict,
    PublishSpec, StatsGroup, StatsReport, StuckPolicy, WorkspacePolicy, DEFAULT_TIMEOUT_SECS,
    LABEL_SOURCE_PROBED, MAX_STUCK_AFTER_SECS, MAX_TASK_BYTES, MIN_STUCK_AFTER_SECS,
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
    /// Repository URL or absolute local path. Required unless --batch supplies it in --defaults.
    #[arg(long, required_unless_present = "batch")]
    pub repo: Option<String>,
    /// Git ref to check out. Required unless --batch supplies it in --defaults.
    #[arg(long = "ref", required_unless_present = "batch")]
    pub git_ref: Option<String>,
    /// Read many tasks from a directory or Markdown checklist. Batch inputs use --defaults
    /// instead of the single-job flags.
    #[arg(
        long,
        value_name = "PATH",
        requires = "key_prefix",
        conflicts_with_all = [
            "task", "task_file", "key", "provider", "model", "fallbacks", "timeout",
            "idle_timeout", "on_stuck", "stuck_after", "after", "after_key", "collect",
            "include_failed", "on_conflict", "allow_empty", "expect_files", "expect_changed",
            "forbid_changed", "min_diff_bytes", "max_diff_bytes", "expect_mode", "require",
            "tolerate", "priority", "group", "allow_spawn", "spawn_inherit", "spawn_repos",
            "workspace", "cache_dirs", "wait"
        ]
    )]
    pub batch: Option<PathBuf>,
    /// Idempotency key prefix for --batch; each job key is <prefix>-<slug>.
    #[arg(long, value_name = "PREFIX", requires = "batch")]
    pub key_prefix: Option<String>,
    /// Shared TOML defaults for --batch.
    #[arg(long, value_name = "FILE", requires = "batch")]
    pub defaults: Option<PathBuf>,
    /// Print the batch plan and submit nothing.
    #[arg(long, requires = "batch")]
    pub dry_run: bool,
    /// Make each batch job depend on the previous one in file order.
    #[arg(long, requires = "batch")]
    pub chain: bool,
    /// Cap the batch's concurrency group at N running jobs (1 to 64).
    #[arg(long, value_name = "N", requires = "batch", value_parser = parse_max_parallel)]
    pub max_parallel: Option<usize>,
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
    /// Action when the job is continuously stuck: none, cancel or retry.
    #[arg(long, value_parser = parse_stuck_policy)]
    pub on_stuck: Option<StuckPolicy>,
    /// Seconds continuously stuck before acting: 60 to 86400.
    #[arg(long, value_parser = parse_stuck_after)]
    pub stuck_after: Option<u64>,
    /// Only run after this job is done (repeatable).
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
    /// How to handle an input that does not apply: fail, markers or skip.
    #[arg(long = "on-conflict", value_name = "MODE", default_value_t = OnConflict::Fail)]
    pub on_conflict: OnConflict,
    /// Expect an empty diff, so no `no changes` note is added when the job is done.
    #[arg(long)]
    pub allow_empty: bool,
    /// A relative path that must exist in the working tree (repeatable).
    #[arg(long = "expect-file")]
    pub expect_files: Vec<String>,
    /// A glob that at least one changed path must match (repeatable).
    #[arg(long = "expect-changed")]
    pub expect_changed: Vec<String>,
    /// A glob that no changed path may match (repeatable).
    #[arg(long = "forbid-changed")]
    pub forbid_changed: Vec<String>,
    /// Minimum exported diff size in bytes.
    #[arg(long)]
    pub min_diff_bytes: Option<u64>,
    /// Maximum exported diff size in bytes.
    #[arg(long)]
    pub max_diff_bytes: Option<u64>,
    /// How unmet expectations affect a successful harness run: suspect or fail.
    #[arg(long = "expect-mode", value_name = "MODE")]
    pub expect_mode: Option<ExpectMode>,
    /// Run only on a node with this label (repeatable).
    #[arg(long = "require")]
    pub require: Vec<String>,
    /// Tolerate this node taint (repeatable).
    #[arg(long = "tolerate")]
    pub tolerate: Vec<String>,
    /// Dispatch priority, 0 (lowest) to 9 (highest); defaults to 5.
    #[arg(
        long,
        value_parser = clap::value_parser!(u8).range(0..=9),
        default_value_t = lu_proto::DEFAULT_PRIORITY
    )]
    pub priority: u8,
    /// Concurrency group name (1-64 chars from A-Z a-z 0-9 . _ -).
    #[arg(long)]
    pub group: Option<String>,
    /// Let this job's harness submit children through its scoped socket.
    #[arg(long)]
    pub allow_spawn: bool,
    /// Let children of this job submit their own children.
    #[arg(long)]
    pub spawn_inherit: bool,
    /// Repository a child may use instead of the parent repo (repeatable).
    #[arg(long = "spawn-repo")]
    pub spawn_repos: Vec<String>,
    /// Workspace policy: ephemeral, keep_on_failure or keep. Omit to use the node default.
    #[arg(long, value_parser = parse_workspace_policy)]
    pub workspace: Option<WorkspacePolicy>,
    /// Relative directory inside the workspace to keep as a build cache (repeatable).
    #[arg(long = "cache-dir")]
    pub cache_dirs: Vec<String>,
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
    /// Workspace policy: ephemeral, keep_on_failure or keep. Omit to use the node default.
    #[arg(long, value_parser = parse_workspace_policy)]
    pub workspace: Option<WorkspacePolicy>,
    /// Relative directory inside the workspace to keep as a build cache (repeatable).
    #[arg(long = "cache-dir")]
    pub cache_dirs: Vec<String>,
    /// Wait for the job to finish.
    #[arg(long)]
    pub wait: bool,
    /// Allow inputs whose patch was changed by controller-side scrubbing or flagged for
    /// high-confidence secret hits. Such patches may need review before applying or publishing.
    #[arg(long)]
    pub allow_scrubbed: bool,
}

#[derive(Debug, clap::Args)]
pub struct RetryArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
    /// Restore blocked dependents transitively, keeping their job ids.
    #[arg(long)]
    pub chain: bool,
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
    /// Set required working-tree paths, replacing the stored list (repeatable).
    #[arg(long = "expect-file")]
    pub expect_files: Vec<String>,
    /// Set changed-path globs, replacing the stored list (repeatable).
    #[arg(long = "expect-changed")]
    pub expect_changed: Vec<String>,
    /// Set forbidden changed-path globs, replacing the stored list (repeatable).
    #[arg(long = "forbid-changed")]
    pub forbid_changed: Vec<String>,
    /// Set the minimum exported diff size in bytes.
    #[arg(long)]
    pub min_diff_bytes: Option<u64>,
    /// Set the maximum exported diff size in bytes.
    #[arg(long)]
    pub max_diff_bytes: Option<u64>,
    /// Set how unmet expectations affect a successful harness run.
    #[arg(long = "expect-mode", value_name = "MODE")]
    pub expect_mode: Option<ExpectMode>,
    /// Replace the stuck policy: none, cancel or retry.
    #[arg(long, value_parser = parse_stuck_policy)]
    pub on_stuck: Option<StuckPolicy>,
    /// Replace the continuous stuck threshold in seconds: 60 to 86400.
    #[arg(long, value_parser = parse_stuck_after)]
    pub stuck_after: Option<u64>,
    /// Replace the dispatch priority, 0 (lowest) to 9 (highest).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=9))]
    pub priority: Option<u8>,
    /// Replace the concurrency group name.
    #[arg(long, conflicts_with = "no_group")]
    pub group: Option<String>,
    /// Clear the concurrency group.
    #[arg(long = "no-group", conflicts_with = "group")]
    pub no_group: bool,
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
    /// Set required working-tree paths, replacing the stored list (repeatable).
    #[arg(long = "expect-file")]
    pub expect_files: Vec<String>,
    /// Set changed-path globs, replacing the stored list (repeatable).
    #[arg(long = "expect-changed")]
    pub expect_changed: Vec<String>,
    /// Set forbidden changed-path globs, replacing the stored list (repeatable).
    #[arg(long = "forbid-changed")]
    pub forbid_changed: Vec<String>,
    /// Set the minimum exported diff size in bytes.
    #[arg(long)]
    pub min_diff_bytes: Option<u64>,
    /// Set the maximum exported diff size in bytes.
    #[arg(long)]
    pub max_diff_bytes: Option<u64>,
    /// Set how unmet expectations affect a successful harness run.
    #[arg(long = "expect-mode", value_name = "MODE")]
    pub expect_mode: Option<ExpectMode>,
    /// Replace the stuck policy: none, cancel or retry.
    #[arg(long, value_parser = parse_stuck_policy)]
    pub on_stuck: Option<StuckPolicy>,
    /// Replace the continuous stuck threshold in seconds: 60 to 86400.
    #[arg(long, value_parser = parse_stuck_after)]
    pub stuck_after: Option<u64>,
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
    /// Replace the dispatch priority, 0 (lowest) to 9 (highest).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=9))]
    pub priority: Option<u8>,
    /// Replace the concurrency group name.
    #[arg(long, conflicts_with = "no_group")]
    pub group: Option<String>,
    /// Clear the concurrency group.
    #[arg(long = "no-group", conflicts_with = "group")]
    pub no_group: bool,
    /// Set or clear whether this job's harness may submit children.
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    pub allow_spawn: Option<bool>,
    /// Set or clear whether children of this job may themselves submit children.
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    pub spawn_inherit: Option<bool>,
    /// Replace the child repository allowlist (repeatable).
    #[arg(long = "spawn-repo", conflicts_with = "no_spawn_repos")]
    pub spawn_repos: Vec<String>,
    /// Clear the child repository allowlist.
    #[arg(long = "no-spawn-repos", conflicts_with = "spawn_repos")]
    pub no_spawn_repos: bool,
    /// Replace the task text with the contents of a UTF-8 file (drafts only).
    #[arg(long = "task-file", value_name = "PATH")]
    pub task_file: Option<PathBuf>,
    /// Replace the source ref, for example a branch, tag or `refs/pull/12/head` (drafts only).
    #[arg(long = "source-ref", value_name = "REF")]
    pub source_ref: Option<String>,
    /// Replace publish settings with a JSON object (drafts only).
    #[arg(long = "publish", value_name = "JSON")]
    pub publish: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct DraftsArgs {
    /// Maximum number of drafts to return.
    #[arg(long, value_parser = parse_limit)]
    pub limit: Option<u32>,
}

#[derive(Debug, clap::Args)]
pub struct ApproveArgs {
    /// Draft id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
    /// Apply edits before approving. Accepts the same flags as `lu edit`.
    #[arg(long)]
    pub edit: bool,
    /// Replace the task text with the contents of a UTF-8 file.
    #[arg(long = "task-file", requires = "edit")]
    pub task_file: Option<PathBuf>,
    /// Replace the source ref.
    #[arg(long = "source-ref", requires = "edit")]
    pub source_ref: Option<String>,
    /// Replace publish settings with a JSON object.
    #[arg(long = "publish", requires = "edit")]
    pub publish: Option<String>,
    /// Change the provider while approving.
    #[arg(long, requires = "edit")]
    pub provider: Option<String>,
    /// Change the model while approving.
    #[arg(long, requires = "edit")]
    pub model: Option<String>,
    /// Change the priority while approving.
    #[arg(long, requires = "edit", value_parser = clap::value_parser!(u8).range(0..=9))]
    pub priority: Option<u8>,
    /// Change the concurrency group while approving.
    #[arg(long, requires = "edit")]
    pub group: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct RejectArgs {
    /// Draft id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
    /// Reason recorded in the audit log. Only coarse categories are stored.
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct LsArgs {
    /// Only show jobs in this state.
    #[arg(long, value_parser = parse_state)]
    pub state: Option<JobState>,
    /// Maximum number of jobs to return.
    #[arg(long, value_parser = parse_limit)]
    pub limit: Option<u32>,
    /// Group jobs by parent instead of showing a flat list.
    #[arg(long)]
    pub tree: bool,
}

#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// Job id.
    #[arg(value_parser = parse_job_id)]
    pub id: JobId,
}

#[derive(Debug, clap::Args)]
pub struct PublishArgs {
    /// Job id whose diff should be published.
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
    /// Leave this job's descendants running instead of cancelling them too.
    #[arg(long)]
    pub keep_children: bool,
    /// Leave blocked dependents waiting instead of cancelling them too.
    #[arg(long)]
    pub keep_dependents: bool,
}

#[derive(Debug, clap::Args)]
pub struct NodesArgs {}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {}

#[derive(Debug, clap::Args)]
pub struct StatsArgs {
    /// Group table rows by node, repository or terminal state.
    #[arg(long, value_name = "GROUP", default_value = "node")]
    pub by: StatsGroup,
    /// Inclusive Unix seconds, UTC date or duration such as `7d`.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
    /// Restrict the report to one configured node.
    #[arg(long, value_name = "NAME", value_parser = parse_node)]
    pub node: Option<NodeName>,
}

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
    /// List the agent's job workspaces instead of its node stats.
    #[arg(long)]
    pub workspaces: bool,
    #[command(subcommand)]
    pub action: Option<NodeAction>,
}

#[derive(Debug, clap::Subcommand)]
pub enum NodeAction {
    /// Ask the agent to delete kept workspaces.
    Clean(NodeCleanArgs),
}

#[derive(Debug, clap::Args)]
pub struct NodeCleanArgs {
    /// Delete every kept workspace that is not running.
    #[arg(long, conflicts_with = "job")]
    pub all: bool,
    /// Delete only this job's workspace, by job id or idempotency key.
    #[arg(long = "job", conflicts_with = "all")]
    pub job: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct NodeTargetArgs {
    /// Node name.
    #[arg(value_parser = parse_node)]
    pub node: NodeName,
}

#[derive(Debug, clap::Args)]
pub struct UncordonArgs {
    /// Node name.
    #[arg(value_parser = parse_node)]
    pub node: NodeName,
    /// Provider to clear. Omit to clear every cordon on the node.
    pub provider: Option<String>,
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

pub async fn drafts(args: DraftsArgs, config: &Path, json: bool) -> ExitCode {
    finish(drafts_inner(args, config, json).await)
}

pub async fn approve(args: ApproveArgs, config: &Path, json: bool) -> ExitCode {
    finish(approve_inner(args, config, json).await)
}

pub async fn reject(args: RejectArgs, config: &Path, json: bool) -> ExitCode {
    finish(reject_inner(args, config, json).await)
}

pub async fn show(args: ShowArgs, config: &Path, json: bool) -> ExitCode {
    finish(show_inner(args, config, json).await)
}

pub async fn publish(args: PublishArgs, config: &Path, json: bool) -> ExitCode {
    finish(publish_inner(args, config, json).await)
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

pub async fn stats(args: StatsArgs, config: &Path, json: bool) -> ExitCode {
    finish(stats_inner(args, config, json).await)
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

pub async fn drain(args: NodeTargetArgs, config: &Path, json: bool) -> ExitCode {
    finish(drain_inner(args, config, json, true).await)
}

pub async fn undrain(args: NodeTargetArgs, config: &Path, json: bool) -> ExitCode {
    finish(drain_inner(args, config, json, false).await)
}

pub async fn uncordon(args: UncordonArgs, config: &Path, json: bool) -> ExitCode {
    finish(uncordon_inner(args, config, json).await)
}

async fn submit_inner(args: SubmitArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    if args.batch.is_some() {
        return submit_batch_inner(args, config, json).await;
    }
    let Some(repo) = args.repo.clone() else {
        return Err("lu: --repo is required".to_owned());
    };
    let Some(git_ref) = args.git_ref.clone() else {
        return Err("lu: --ref is required".to_owned());
    };
    let task = read_task(&args)?;
    let client = connect(config)?;
    let Some(after) = resolve_after(&client, &args.after, &args.after_key).await? else {
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    let expect = ExpectFlags {
        files: args.expect_files,
        changed: args.expect_changed,
        forbid_changed: args.forbid_changed,
        min_diff_bytes: args.min_diff_bytes,
        max_diff_bytes: args.max_diff_bytes,
        mode: args.expect_mode,
    };
    let spec = JobSpec {
        source: lu_proto::SourceSpec {
            repo: repo.clone(),
            git_ref: git_ref.clone(),
            base: None,
        },
        repo,
        git_ref,
        base_commit: None,
        task,
        harness: args.harness,
        provider: args.provider,
        priority: args.priority,
        group: args.group,
        allow_spawn: args.allow_spawn,
        spawn_inherit: args.spawn_inherit,
        spawn_repos: args.spawn_repos,
        workspace: args.workspace,
        cache_dirs: args.cache_dirs,
        context: None,
        origin: None,
        origin_ref: None,
        untrusted_context: false,
        draft: false,
        publish: None,
        model: args.model,
        timeout_secs: args.timeout,
        idle_timeout_secs: args.idle_timeout,
        on_stuck: args.on_stuck,
        stuck_after_secs: args.stuck_after,
        idempotency_key: args.key,
        after,
        collect: args.collect.unwrap_or_default(),
        include_failed: args.include_failed,
        on_conflict: args.on_conflict,
        allow_empty: args.allow_empty,
        requires: args.require,
        tolerates: args.tolerate,
        fallbacks: args.fallbacks,
        expect: expect.spec(),
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
        schedule: None,
    };
    spec.validate().map_err(|error| error.to_string())?;

    submit_and_report(&client, &spec, json, args.wait).await
}

async fn submit_batch_inner(
    args: SubmitArgs,
    config: &Path,
    json: bool,
) -> Result<ExitCode, String> {
    let path = args
        .batch
        .expect("clap requires --batch for a batch submit");
    let prefix = args
        .key_prefix
        .as_deref()
        .expect("clap requires --key-prefix with --batch");
    let options = crate::batch::BatchOptions {
        path: &path,
        key_prefix: prefix,
        defaults: args.defaults.as_deref(),
        repo: args.repo.as_deref(),
        git_ref: args.git_ref.as_deref(),
        chain: args.chain,
        max_parallel: args.max_parallel,
    };
    let plan = match crate::batch::plan(&options) {
        Ok(plan) => plan,
        Err(errors) => {
            for error in errors {
                eprintln!("lu: {error}");
            }
            return Ok(ExitCode::from(EXIT_USAGE));
        }
    };
    if args.dry_run {
        crate::batch::print_plan(&plan, json)?;
        return Ok(ExitCode::SUCCESS);
    }

    let client = connect(config)?;
    let prepared = match crate::batch::prepare(&client, &plan).await {
        Ok(prepared) => prepared,
        Err(failure) => {
            eprintln!("lu: {}", failure.reason);
            return Ok(ExitCode::from(if failure.preflight {
                EXIT_USAGE
            } else {
                EXIT_ERROR
            }));
        }
    };
    match crate::batch::submit_prepared(&client, &prepared).await {
        Ok(outcomes) => {
            crate::batch::print_outcomes(&outcomes, json)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(failure) => {
            crate::batch::print_outcomes(&failure.submitted, json)?;
            eprintln!(
                "lu: submitted {} of {} jobs; not submitted: {}; error: {}",
                failure.submitted.len(),
                plan.len(),
                if failure.not_submitted.is_empty() {
                    "-".to_owned()
                } else {
                    failure.not_submitted.join(", ")
                },
                failure.reason
            );
            Ok(ExitCode::from(EXIT_ERROR))
        }
    }
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
        if view.patch_scrubbed && !args.allow_scrubbed {
            eprintln!(
                "lu: job {} has a scrubbed patch ({} scrubbed value(s)); pass --allow-scrubbed to collect it",
                view.id, view.scrubbed_hits
            );
            return Ok(ExitCode::from(EXIT_USAGE));
        }
        if view.patch_secret_hits.count > 0 && !args.allow_scrubbed {
            eprintln!(
                "lu: job {} has {} high-confidence patch secret hit(s) (rules: {}); pass --allow-scrubbed to collect it",
                view.id,
                view.patch_secret_hits.count,
                view.patch_secret_hits.rules.join(", ")
            );
            return Ok(ExitCode::from(EXIT_USAGE));
        }
        if view.state == JobState::Failed && view.diff_bytes.unwrap_or(0) > 0 {
            include_failed = true;
        }
        if !after.contains(&view.id) {
            after.push(view.id);
        }
    }

    let spec = JobSpec {
        source: lu_proto::SourceSpec {
            repo: args.repo.clone(),
            git_ref: args.git_ref.clone(),
            base: None,
        },
        repo: args.repo,
        git_ref: args.git_ref,
        base_commit: None,
        task,
        harness: "opencode".to_owned(),
        provider: args.provider,
        priority: lu_proto::DEFAULT_PRIORITY,
        group: None,
        allow_spawn: false,
        spawn_inherit: false,
        spawn_repos: Vec::new(),
        workspace: args.workspace,
        cache_dirs: args.cache_dirs,
        context: None,
        origin: None,
        origin_ref: None,
        untrusted_context: false,
        draft: false,
        publish: None,
        model: args.model,
        timeout_secs: args.timeout,
        idle_timeout_secs: None,
        on_stuck: None,
        stuck_after_secs: None,
        idempotency_key: args.key,
        after,
        collect: CollectMode::Both,
        include_failed,
        on_conflict: OnConflict::Markers,
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        fallbacks: Vec::new(),
        expect: None,
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
        schedule: None,
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
    if !view.state.is_retryable() {
        eprintln!(
            "lu: job {} is {}; retry requires failed, cancelled, expired or deadline_missed",
            view.id,
            state_name(view.state)
        );
        return Ok(ExitCode::from(EXIT_USAGE));
    }

    let expect = ExpectFlags {
        files: args.expect_files,
        changed: args.expect_changed,
        forbid_changed: args.forbid_changed,
        min_diff_bytes: args.min_diff_bytes,
        max_diff_bytes: args.max_diff_bytes,
        mode: args.expect_mode,
    };
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
        spec.collect = collect;
    }
    if args.include_failed {
        spec.include_failed = true;
    }
    if let Some(priority) = args.priority {
        spec.priority = priority;
    }
    if let Some(group) = args.group {
        spec.group = Some(group);
    } else if args.no_group {
        spec.group = None;
    }
    if let Some(on_conflict) = args.on_conflict {
        spec.on_conflict = on_conflict;
    }
    if let Some(on_stuck) = args.on_stuck {
        spec.on_stuck = Some(on_stuck);
    }
    if let Some(stuck_after) = args.stuck_after {
        spec.stuck_after_secs = Some(stuck_after);
    }
    let merged_expect = expect.merged(spec.expect.as_ref());
    spec.expect = merged_expect;
    // The job keeps its id; --key still updates the idempotency mapping for this attempt.
    spec.idempotency_key = args.key;
    spec.validate().map_err(|error| error.to_string())?;

    let chain = args.chain;
    let reply = client
        .retry_with_dependents(&args.id, &spec, chain)
        .await
        .map_err(|error| error.to_string())?;
    if json {
        let text = serde_json::to_string(&reply).map_err(|error| error.to_string())?;
        print_raw(&text);
    } else {
        println!(
            "retried {}; requeued {} dependents",
            reply.job.id,
            reply.requeued.len()
        );
        if reply.waiting_dependents > 0 {
            println!("waiting_dependents {}", reply.waiting_dependents);
        }
    }
    if args.wait {
        let final_view = wait_for_job(&client, &reply.job.id).await?;
        eprintln!("state: {}", state_name(final_view.state));
        if json {
            let raw = client
                .get_job_raw(&reply.job.id)
                .await
                .map_err(|error| error.to_string())?;
            print_raw(&raw);
        }
        return Ok(if final_view.state == JobState::Done {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(EXIT_UNSUCCESSFUL)
        });
    }
    Ok(ExitCode::SUCCESS)
}

fn task_from_file(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() > MAX_TASK_BYTES {
        return Err(format!(
            "task file is {} bytes, limit is {MAX_TASK_BYTES}",
            bytes.len()
        ));
    }
    String::from_utf8(bytes).map_err(|error| format!("task file is not UTF-8: {error}"))
}

fn publish_from_json(raw: &str) -> Result<PublishSpec, String> {
    serde_json::from_str(raw).map_err(|error| format!("invalid --publish JSON: {error}"))
}

/// Builds only the fields an operator explicitly supplied. Draft edits accept task text, source
/// ref and publish settings in addition to the fields shared with queued jobs.
fn draft_edit(
    task_file: Option<&Path>,
    source_ref: Option<String>,
    publish: Option<&str>,
) -> Result<Option<JobEdit>, String> {
    let task = task_file.map(task_from_file).transpose()?;
    let publish = publish.map(publish_from_json).transpose()?;
    if task.is_none() && source_ref.is_none() && publish.is_none() {
        return Ok(None);
    }
    Ok(Some(JobEdit {
        task,
        source_ref,
        publish,
        ..JobEdit::default()
    }))
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
    let expect = ExpectFlags {
        files: args.expect_files,
        changed: args.expect_changed,
        forbid_changed: args.forbid_changed,
        min_diff_bytes: args.min_diff_bytes,
        max_diff_bytes: args.max_diff_bytes,
        mode: args.expect_mode,
    };
    let draft = draft_edit(
        args.task_file.as_deref(),
        args.source_ref,
        args.publish.as_deref(),
    )?;
    let edit = JobEdit {
        task: draft.as_ref().and_then(|edit| edit.task.clone()),
        source_ref: draft.as_ref().and_then(|edit| edit.source_ref.clone()),
        publish: draft.as_ref().and_then(|edit| edit.publish.clone()),
        provider: args.provider,
        model: args.model,
        fallbacks: (!args.fallbacks.is_empty() || args.no_fallbacks).then_some(args.fallbacks),
        requires: (!args.require.is_empty() || args.no_require).then_some(args.require),
        tolerates: (!args.tolerate.is_empty() || args.no_tolerate).then_some(args.tolerate),
        after,
        collect: args.collect,
        include_failed: args.include_failed,
        expect: expect.edit(),
        on_conflict: args.on_conflict,
        priority: args.priority,
        group: if args.group.is_some() {
            args.group.map(Some)
        } else if args.no_group {
            Some(None)
        } else {
            None
        },
        on_stuck: args.on_stuck,
        stuck_after_secs: args.stuck_after,
        allow_spawn: args.allow_spawn,
        spawn_inherit: args.spawn_inherit,
        spawn_repos: if !args.spawn_repos.is_empty() {
            Some(args.spawn_repos)
        } else if args.no_spawn_repos {
            Some(Vec::new())
        } else {
            None
        },
        schedule: None,
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
        .list_jobs(if args.tree { None } else { args.state }, {
            if args.tree {
                None
            } else {
                args.limit
            }
        })
        .await
        .map_err(|error| error.to_string())?;
    if args.tree {
        print_job_tree(&page.jobs);
        return Ok(ExitCode::SUCCESS);
    }
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
                job.blocked_reason
                    .as_deref()
                    .map(display_field)
                    .unwrap_or_else(|| "-".to_owned()),
                outcome_text(job.outcome_note.as_deref()),
            ]
        })
        .collect();
    print_table(
        &[
            "ID", "STATE", "NODE", "ATTEMPT", "AFTER", "CREATED", "BLOCKED", "NOTE",
        ],
        rows,
    );
    Ok(ExitCode::SUCCESS)
}

async fn drafts_inner(args: DraftsArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let page = client
        .drafts(args.limit)
        .await
        .map_err(|error| error.to_string())?;
    if json {
        let raw = serde_json::to_string(&page.jobs).map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let rows = page
        .jobs
        .iter()
        .map(|draft| {
            vec![
                draft.id.to_string(),
                draft
                    .spec
                    .origin
                    .as_deref()
                    .map(display_field)
                    .unwrap_or_else(|| "-".to_owned()),
                draft
                    .spec
                    .origin_ref
                    .as_deref()
                    .map(display_field)
                    .unwrap_or_else(|| "-".to_owned()),
                draft
                    .spec
                    .task
                    .lines()
                    .next()
                    .map(|line| truncate_chars(line, 80))
                    .unwrap_or_default(),
                time(draft.created_at),
            ]
        })
        .collect();
    print_table(&["ID", "ORIGIN", "ORIGIN_REF", "TASK", "CREATED"], rows);
    Ok(ExitCode::SUCCESS)
}

async fn approve_inner(args: ApproveArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let mut edit = if args.edit {
        draft_edit(
            args.task_file.as_deref(),
            args.source_ref,
            args.publish.as_deref(),
        )?
    } else {
        if args.task_file.is_some() || args.source_ref.is_some() || args.publish.is_some() {
            return Err("lu: edit flags require --edit".to_owned());
        }
        None
    };
    if edit.is_none()
        && (args.provider.is_some()
            || args.model.is_some()
            || args.priority.is_some()
            || args.group.is_some())
    {
        edit = Some(JobEdit::default());
    }
    if let Some(edit) = edit.as_mut() {
        edit.provider = args.provider;
        edit.model = args.model;
        edit.priority = args.priority;
        edit.group = args.group.map(Some);
    }
    let client = connect(config)?;
    let view = client
        .approve_draft(&args.id, edit.as_ref())
        .await
        .map_err(|error| error.to_string())?;
    if json {
        print_raw(&serde_json::to_string(&view).map_err(|error| error.to_string())?);
    } else {
        println!("{} {}", view.id, state_name(view.state));
    }
    Ok(ExitCode::SUCCESS)
}

async fn reject_inner(args: RejectArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let view = client
        .reject_draft(&args.id, args.reason.as_deref())
        .await
        .map_err(|error| error.to_string())?;
    if json {
        print_raw(&serde_json::to_string(&view).map_err(|error| error.to_string())?);
    } else {
        println!("{} {}", view.id, state_name(view.state));
    }
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

async fn publish_inner(args: PublishArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let view = client
        .publish(&args.id)
        .await
        .map_err(|error| error.to_string())?;
    if json {
        print_raw(&serde_json::to_string(&view).map_err(|error| error.to_string())?);
    } else {
        match &view.publish {
            Some(record) => println!(
                "{} {} {} ({})",
                record.detail, record.remote, record.branch, record.target
            ),
            None => println!("{} pending", view.id),
        }
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
            .cancel_raw_with_options(&args.id, args.keep_children, args.keep_dependents)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let view = client
        .cancel_with_options(&args.id, args.keep_children, args.keep_dependents)
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
            let cordons: Vec<String> = node
                .cordon_details
                .iter()
                .map(|cordon| format!("!{}", cordon.provider))
                .collect();
            vec![
                node.name.to_string(),
                node.version
                    .as_deref()
                    .map(display_field)
                    .unwrap_or_else(|| "-".to_owned()),
                harness_name(node.harness),
                match node.harness_ok {
                    Some(false) => "bad".to_owned(),
                    Some(true) => "ok".to_owned(),
                    None => "-".to_owned(),
                },
                node.disk_free_pct
                    .map(|pct| format!("{pct}%"))
                    .unwrap_or_else(|| "-".to_owned()),
                node_health_reason(node),
                labels_cell(&labels, 40),
                labels_cell(&cordons, 24),
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
            "HARNESS_OK",
            "DISK_FREE",
            "HEALTH",
            "LABELS",
            "CORDONS",
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

async fn stats_inner(args: StatsArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    let since = args.since.as_deref();
    if json {
        let raw = client
            .stats_raw(args.by, since, args.node.as_ref())
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    let report = client
        .stats(args.by, since, args.node.as_ref())
        .await
        .map_err(|error| error.to_string())?;
    print_stats(&report);
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
    if args.workspaces && args.action.is_some() {
        eprintln!("lu: --workspaces cannot be combined with clean");
        return Ok(ExitCode::from(EXIT_USAGE));
    }
    if json {
        let raw = client
            .node_stats_raw(&args.node)
            .await
            .map_err(|error| error.to_string())?;
        print_raw(&raw);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(NodeAction::Clean(clean)) = &args.action {
        if !clean.all && clean.job.is_none() {
            eprintln!("lu: clean needs --all or --job <key>");
            return Ok(ExitCode::from(EXIT_USAGE));
        }
        let job = match clean.job.as_deref() {
            Some(value) => Some(resolve_job_ref(&client, value).await?),
            None => None,
        };
        let result = client
            .clean_workspaces(&args.node, clean.all, job.as_ref())
            .await
            .map_err(|error| error.to_string())?;
        println!(
            "queued {}  refused_running {}  missing {}",
            result.queued, result.refused_running, result.missing
        );
        return Ok(ExitCode::SUCCESS);
    }
    if args.workspaces {
        let nodes = client.nodes().await.map_err(|error| error.to_string())?;
        let view = nodes
            .nodes
            .iter()
            .find(|node| node.name == args.node)
            .ok_or_else(|| format!("node {} not found", args.node))?;
        print_workspaces(&view.workspaces);
        return Ok(ExitCode::SUCCESS);
    }
    let stats = client
        .node_stats(&args.node)
        .await
        .map_err(|error| error.to_string())?;
    let nodes = client.nodes().await.map_err(|error| error.to_string())?;
    let view = nodes
        .nodes
        .iter()
        .find(|node| node.name == args.node)
        .ok_or_else(|| format!("node {} not found", args.node))?;
    print_node_stats(&args.node, &stats);
    print_cordons(view);
    Ok(ExitCode::SUCCESS)
}

/// Resolves a job id or idempotency key for `--job`. A missing key is a usage error.
async fn resolve_job_ref(client: &Client, value: &str) -> Result<JobId, String> {
    if let Ok(id) = JobId::parse(value) {
        return Ok(id);
    }
    match client
        .job_by_key(value)
        .await
        .map_err(|error| error.to_string())?
    {
        Some(view) => Ok(view.id),
        None => {
            eprintln!("lu: no job with idempotency key {value:?}");
            Err(format!("unknown job reference {value:?}"))
        }
    }
}

fn print_workspaces(workspaces: &[lu_proto::WorkspaceView]) {
    if workspaces.is_empty() {
        println!("workspaces  -");
        return;
    }
    let rows = workspaces
        .iter()
        .map(|workspace| {
            vec![
                printable(&workspace.job_key),
                workspace.state.as_str().to_owned(),
                workspace.bytes.to_string(),
                short_duration(workspace.age_secs),
                workspace.policy.as_str().to_owned(),
            ]
        })
        .collect();
    print_table(&["JOB", "STATE", "BYTES", "AGE", "POLICY"], rows);
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
    for line in graph_lines(&graph, width) {
        if colour {
            print_graph_line(&line);
        } else {
            println!("{}", line.text());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `lu graph` keeps task titles; the dashboard calls `graph::render` directly without them.
fn graph_lines(graph: &Graph, width: usize) -> Vec<graph::GraphLine> {
    graph::render(graph, width, true).lines
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
        Tone::Done | Tone::NodeIdle | Tone::JobDone => Some("\u{1b}[2;32m"),
        Tone::Blocked => Some("\u{1b}[38;5;208m"),
        Tone::NodeBusy | Tone::JobQueued => Some("\u{1b}[33m"),
        Tone::NodeDrained => Some("\u{1b}[1;34m"),
        Tone::NodeOffline | Tone::JobFailed => Some("\u{1b}[1;31m"),
        Tone::JobRunning => Some("\u{1b}[1;92m"),
        Tone::JobCancelled => Some("\u{1b}[2;90m"),
    }
}

async fn drain_inner(
    args: NodeTargetArgs,
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

async fn uncordon_inner(args: UncordonArgs, config: &Path, json: bool) -> Result<ExitCode, String> {
    let client = connect(config)?;
    match client.uncordon(&args.node, args.provider.as_deref()).await {
        Ok(removed) => {
            if json {
                print_raw(&serde_json::json!({ "removed": removed }).to_string());
            } else if removed.is_empty() {
                println!("uncordoned {}: -", args.node);
            } else {
                println!("uncordoned {}: {}", args.node, removed.join(", "));
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(ApiError::NotFound) => Err(format!("node '{}' not found", args.node.as_str())),
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
                .ok_or_else(|| {
                    "LU_TOKEN_FILE must be set when LU_URL is set (LU_CA_FILE is optional)"
                        .to_owned()
                })?;
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
    Err("no controller URL: set LU_URL and LU_TOKEN_FILE (LU_CA_FILE is optional), or write url and token_file (optional ca_file) in ~/.config/lu/operator.toml or /etc/lu/operator.toml; --config never supplies operator credentials".to_owned())
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

/// Prints the job list as an indentation tree. Jobs whose parent is absent from the page are
/// shown as roots, so a limited view never silently drops a branch.
fn print_job_tree(jobs: &[JobView]) {
    use std::collections::{BTreeMap, BTreeSet};

    let mut children: BTreeMap<Option<String>, Vec<&JobView>> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for job in jobs {
        ids.insert(job.id.to_string());
    }
    for job in jobs {
        let parent = job
            .parent_job_id
            .as_ref()
            .map(ToString::to_string)
            .filter(|parent| ids.contains(parent));
        children.entry(parent).or_default().push(job);
    }
    fn walk(
        parent: Option<&str>,
        children: &BTreeMap<Option<String>, Vec<&JobView>>,
        depth: usize,
        seen: &mut BTreeSet<String>,
    ) {
        let Some(rows) = children.get(&parent.map(str::to_owned)) else {
            return;
        };
        for job in rows {
            if !seen.insert(job.id.to_string()) {
                continue;
            }
            let marker = if job.parent_job_id.is_none() {
                ""
            } else {
                "└─ "
            };
            println!(
                "{}{}{}  {:<9} {}",
                "  ".repeat(depth),
                marker,
                job.id,
                state_name(job.state),
                job_title(&job.spec.task)
            );
            walk(Some(job.id.as_str()), children, depth + 1, seen);
        }
    }
    if jobs.is_empty() {
        println!("no jobs");
        return;
    }
    let mut seen = BTreeSet::new();
    walk(None, &children, 0, &mut seen);
    // A cycle cannot be created through the scope API, but a hand-edited database could contain
    // one. Show those jobs rather than hiding them.
    for job in jobs {
        if seen.insert(job.id.to_string()) && job.parent_job_id.is_some() {
            println!(
                "? {}  {:<9} {}",
                job.id,
                state_name(job.state),
                job_title(&job.spec.task)
            );
        }
    }
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
            "parent",
            view.parent_job_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "root",
            view.root_job_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        ("depth", view.depth.to_string()),
        (
            "origin",
            view.spec
                .origin
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "origin_ref",
            view.spec
                .origin_ref
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        ("untrusted_context", view.spec.untrusted_context.to_string()),
        (
            "context_bytes",
            view.spec
                .context
                .as_ref()
                .map(|text| text.len().to_string())
                .unwrap_or_else(|| "-".to_owned()),
        ),
        ("allow_spawn", view.spec.allow_spawn.to_string()),
        ("spawn_inherit", view.spec.spawn_inherit.to_string()),
        (
            "spawn_repos",
            if view.spec.spawn_repos.is_empty() {
                "-".to_owned()
            } else {
                view.spec.spawn_repos.join(", ")
            },
        ),
        ("priority", view.spec.priority.to_string()),
        (
            "group",
            view.spec
                .group
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
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
        (
            "staged_commit",
            view.staged_commit
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "blocked_reason",
            view.blocked_reason
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "blocked_until",
            optional_time(
                view.blocked_until
                    .and_then(|value| u64::try_from(value).ok()),
            ),
        ),
        ("outcome_note", outcome_text(view.outcome_note.as_deref())),
        (
            "diff_bytes",
            view.diff_bytes
                .map(|bytes| bytes.to_string())
                .unwrap_or_else(|| "-".to_owned()),
        ),
        ("scrubbed_hits", view.scrubbed_hits.to_string()),
        ("patch_scrubbed", view.patch_scrubbed.to_string()),
        (
            "patch_secret_hits",
            if view.patch_secret_hits.count == 0 {
                "0".to_owned()
            } else {
                format!(
                    "{} ({})",
                    view.patch_secret_hits.count,
                    view.patch_secret_hits.rules.join(", ")
                )
            },
        ),
        (
            "publish_target",
            view.publish_target
                .as_deref()
                .map(display_field)
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "publish",
            view.publish
                .as_ref()
                .map(|p| {
                    format!(
                        "{} {} {} ({})",
                        display_field(&p.detail),
                        display_field(&p.remote),
                        display_field(&p.branch),
                        p.target
                    )
                })
                .unwrap_or_else(|| "-".to_owned()),
        ),
        (
            "audit_write_errors",
            view.audit_write_errors
                .map(|count| count.to_string())
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
        ("waiting_dependents", status.waiting_dependents.to_string()),
        ("oldest_queued_secs", status.oldest_queued_secs.to_string()),
        ("nodes", status.nodes.to_string()),
        ("nodes_online", status.nodes_online.to_string()),
        ("lease_secs", status.lease_secs.to_string()),
        ("audit_write_errors", status.audit_write_errors.to_string()),
        ("scope_enabled", status.scope_enabled.to_string()),
        ("scope_max_depth", status.scope_max_depth.to_string()),
        (
            "scope_max_children_per_job",
            status.scope_max_children_per_job.to_string(),
        ),
        (
            "scope_max_descendants_per_root",
            status.scope_max_descendants_per_root.to_string(),
        ),
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

fn stats_value(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn print_stats_nodes(rows: &[lu_proto::NodeContribution]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            let s = &row.stats;
            vec![
                display_field(row.node.as_str()),
                stats_value(s.states.get(JobState::Done.as_str()).copied()),
                stats_value(s.states.get(JobState::Failed.as_str()).copied()),
                stats_value(s.states.get(JobState::Cancelled.as_str()).copied()),
                stats_value(s.states.get(JobState::Expired.as_str()).copied()),
                s.retries.to_string(),
                s.busy_seconds.to_string(),
                stats_value(s.runtime.median_seconds),
                stats_value(s.runtime.p90_seconds),
                stats_value(s.queue_wait.median_seconds),
                stats_value(s.queue_wait.p90_seconds),
                s.last_finished_at
                    .map(|stamp| time(stamp.max(0) as u64))
                    .unwrap_or_else(|| "-".to_owned()),
            ]
        })
        .collect()
}

fn print_stats(report: &StatsReport) {
    match report.by {
        StatsGroup::Node => print_table(
            &[
                "NODE",
                "DONE",
                "FAILED",
                "CANCELLED",
                "EXPIRED",
                "RETRIES",
                "BUSY_S",
                "RUN_MEDIAN_S",
                "RUN_P90_S",
                "WAIT_MEDIAN_S",
                "WAIT_P90_S",
                "LAST_FINISHED",
            ],
            print_stats_nodes(&report.nodes),
        ),
        StatsGroup::Repo => {
            let rows = report
                .fleet
                .top_repositories
                .iter()
                .map(|repo| vec![display_field(&repo.repository), repo.jobs.to_string()])
                .collect();
            print_table(&["REPOSITORY", "JOBS"], rows);
        }
        StatsGroup::State => {
            let rows = [
                JobState::Done,
                JobState::Failed,
                JobState::Cancelled,
                JobState::Expired,
            ]
            .into_iter()
            .map(|state| {
                vec![
                    state.as_str().to_owned(),
                    report
                        .fleet
                        .states
                        .get(state.as_str())
                        .copied()
                        .unwrap_or(0)
                        .to_string(),
                ]
            })
            .collect();
            print_table(&["STATE", "JOBS"], rows);
        }
    }
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
        (
            "harness_ok",
            match node.harness_ok {
                Some(false) => "bad".to_owned(),
                Some(true) => "ok".to_owned(),
                None => "-".to_owned(),
            },
        ),
        (
            "disk_free_pct",
            node.disk_free_pct
                .map(|pct| format!("{pct}%"))
                .unwrap_or_else(|| "-".to_owned()),
        ),
        ("health", node_health_reason(node)),
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
    println!();
    print_pairs(&[
        ("workspace_bytes", stats.workspace_bytes.to_string()),
        ("workspace_count", stats.workspace_count.to_string()),
        (
            "workspace_reclaimed_bytes_total",
            stats.workspace_reclaimed_bytes_total.to_string(),
        ),
        (
            "workspace_evictions_total",
            stats.workspace_evictions_total.to_string(),
        ),
        (
            "workspace_delete_errors_total",
            stats.workspace_delete_errors_total.to_string(),
        ),
        ("gc_removed_total", stats.gc_removed_total.to_string()),
        (
            "gc_bytes_freed_total",
            stats.gc_bytes_freed_total.to_string(),
        ),
    ]);
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

fn short_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, secs % 3600 / 60)
    }
}

fn print_cordons(node: &NodeView) {
    println!("cordons");
    if node.cordon_details.is_empty() {
        println!("  -");
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for c in &node.cordon_details {
        let age = now.saturating_sub(c.set_at.max(0) as u64);
        let ago = if age == 0 {
            "just now".to_owned()
        } else {
            format!("{} ago", short_duration(age))
        };
        let state = match (c.until, c.lifts_after_healthy_heartbeats) {
            (Some(until), _) if until > now as i64 => {
                format!("{} left", short_duration((until - now as i64) as u64))
            }
            (Some(_), _) => "expired".to_owned(),
            (None, Some(count)) => format!("lifts on {count} healthy"),
            (None, None) => "lifts on healthy".to_owned(),
        };
        println!("  {} [{}] {ago}; {state}", c.provider, c.source);
        println!("      {}", printable(&c.reason));
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

pub(crate) fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
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

/// Why the latest heartbeat makes a node ineligible, using the controller's own threshold.
pub(crate) fn node_health_reason(node: &NodeView) -> String {
    node.health().text
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
        Some("expect_failed") => "expect failed".to_owned(),
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

fn parse_workspace_policy(value: &str) -> Result<WorkspacePolicy, String> {
    WorkspacePolicy::parse(value).map_err(|error| error.to_string())
}

/// The shared shape of the six expectation flags on submit, retry and edit.
struct ExpectFlags {
    files: Vec<String>,
    changed: Vec<String>,
    forbid_changed: Vec<String>,
    min_diff_bytes: Option<u64>,
    max_diff_bytes: Option<u64>,
    mode: Option<ExpectMode>,
}

impl ExpectFlags {
    fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.changed.is_empty()
            && self.forbid_changed.is_empty()
            && self.min_diff_bytes.is_none()
            && self.max_diff_bytes.is_none()
            && self.mode.is_none()
    }

    fn spec(&self) -> Option<ExpectSpec> {
        if self.is_empty() {
            return None;
        }
        Some(ExpectSpec {
            files: self.files.clone(),
            changed: self.changed.clone(),
            forbid_changed: self.forbid_changed.clone(),
            min_diff_bytes: self.min_diff_bytes,
            max_diff_bytes: self.max_diff_bytes,
            mode: self.mode.unwrap_or_default(),
        })
    }

    fn merged(&self, base: Option<&ExpectSpec>) -> Option<ExpectSpec> {
        if self.is_empty() {
            return base.cloned();
        }
        let mut next = base.cloned().unwrap_or_default();
        if !self.files.is_empty() {
            next.files = self.files.clone();
        }
        if !self.changed.is_empty() {
            next.changed = self.changed.clone();
        }
        if !self.forbid_changed.is_empty() {
            next.forbid_changed = self.forbid_changed.clone();
        }
        if let Some(min) = self.min_diff_bytes {
            next.min_diff_bytes = Some(min);
        }
        if let Some(max) = self.max_diff_bytes {
            next.max_diff_bytes = Some(max);
        }
        if let Some(mode) = self.mode {
            next.mode = mode;
        }
        Some(next)
    }

    fn edit(&self) -> Option<ExpectEdit> {
        if self.is_empty() {
            return None;
        }
        Some(ExpectEdit {
            files: (!self.files.is_empty()).then(|| self.files.clone()),
            changed: (!self.changed.is_empty()).then(|| self.changed.clone()),
            forbid_changed: (!self.forbid_changed.is_empty()).then(|| self.forbid_changed.clone()),
            min_diff_bytes: self.min_diff_bytes,
            max_diff_bytes: self.max_diff_bytes,
            mode: self.mode,
        })
    }
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
        let expected = JobState::ALL
            .iter()
            .map(|state| state.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown job state {normalized:?}; expected {expected}")
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

fn parse_max_parallel(value: &str) -> Result<usize, String> {
    let max: usize = value
        .parse()
        .map_err(|_| format!("max_parallel must be a positive integer (got {value:?})"))?;
    if !(1..=lu_proto::MAX_GROUP_RUNNING).contains(&max) {
        return Err(format!(
            "max_parallel must be 1 to {} (got {max})",
            lu_proto::MAX_GROUP_RUNNING
        ));
    }
    Ok(max)
}

fn parse_stuck_policy(value: &str) -> Result<StuckPolicy, String> {
    StuckPolicy::parse(value).map_err(|error| error.to_string())
}

fn parse_stuck_after(value: &str) -> Result<u64, String> {
    let secs: u64 = value
        .parse()
        .map_err(|_| format!("stuck after must be a whole number of seconds (got {value:?})"))?;
    if !(MIN_STUCK_AFTER_SECS..=MAX_STUCK_AFTER_SECS).contains(&secs) {
        return Err(format!(
            "stuck after must be {MIN_STUCK_AFTER_SECS} to {MAX_STUCK_AFTER_SECS} seconds (got {secs})"
        ));
    }
    Ok(secs)
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
mod graph_tests {
    use lu_tui::graph::{Job, State};

    use super::graph_lines;

    #[test]
    fn the_graph_command_draws_titles_on_one_row_per_chain() {
        let graph = lu_tui::graph::Graph::build(&[
            Job {
                id: "AAAAAAAAAA".to_owned(),
                state: State::Done,
                title: "build".to_owned(),
                after: Vec::new(),
            },
            Job {
                id: "BBBBBBBBBB".to_owned(),
                state: State::Queued,
                title: "test".to_owned(),
                after: vec!["AAAAAAAAAA".to_owned()],
            },
        ]);
        let lines = graph_lines(&graph, 120);
        let text: String = lines
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(lines.len(), 1, "{text}");
        assert!(text.contains("✓ AAAAA build ───▶ ○ BBBBB test"), "{text}");
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
        Submit(Box<SubmitArgs>),
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

#[cfg(test)]
mod expect_cli_tests {
    use clap::Parser;
    use lu_proto::ExpectMode;

    use super::{EditArgs, RetryArgs, SubmitArgs};

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Debug, clap::Subcommand)]
    enum Command {
        Submit(Box<SubmitArgs>),
        Retry(RetryArgs),
        Edit(EditArgs),
    }

    #[test]
    fn submit_parses_every_expectation_flag() {
        let cli = Cli::try_parse_from([
            "lu",
            "submit",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--task",
            "go",
            "--expect-file",
            "REVIEW.md",
            "--expect-changed",
            "src/**",
            "--forbid-changed",
            "Cargo.lock",
            "--min-diff-bytes",
            "1",
            "--max-diff-bytes",
            "5242880",
            "--expect-mode",
            "fail",
        ])
        .unwrap();
        let Command::Submit(args) = cli.command else {
            panic!("expected submit");
        };
        assert_eq!(args.expect_files, ["REVIEW.md"]);
        assert_eq!(args.expect_changed, ["src/**"]);
        assert_eq!(args.forbid_changed, ["Cargo.lock"]);
        assert_eq!(args.min_diff_bytes, Some(1));
        assert_eq!(args.max_diff_bytes, Some(5_242_880));
        assert_eq!(args.expect_mode, Some(ExpectMode::Fail));

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
        assert!(args.expect_files.is_empty());
        assert!(args.expect_mode.is_none());
    }

    #[test]
    fn retry_and_edit_parse_expectation_flags() {
        let id = lu_proto::JobId::new().to_string();
        let cli = Cli::try_parse_from([
            "lu",
            "retry",
            &id,
            "--expect-file",
            "REVIEW.md",
            "--expect-mode",
            "suspect",
        ])
        .unwrap();
        let Command::Retry(args) = cli.command else {
            panic!("expected retry");
        };
        assert_eq!(args.expect_files, ["REVIEW.md"]);
        assert_eq!(args.expect_mode, Some(ExpectMode::Suspect));

        let cli = Cli::try_parse_from([
            "lu",
            "edit",
            &id,
            "--expect-changed",
            "src/**",
            "--max-diff-bytes",
            "10",
        ])
        .unwrap();
        let Command::Edit(args) = cli.command else {
            panic!("expected edit");
        };
        assert_eq!(args.expect_changed, ["src/**"]);
        assert_eq!(args.max_diff_bytes, Some(10));
    }

    #[test]
    fn an_invalid_expect_mode_is_a_clap_error() {
        let error = Cli::try_parse_from([
            "lu",
            "submit",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--task",
            "go",
            "--expect-mode",
            "nope",
        ])
        .unwrap_err();
        assert!(error.to_string().contains("expect-mode"), "{error}");
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn parse_state_lists_every_wire_state_in_its_error() {
        assert_eq!(parse_state("draft").unwrap(), JobState::Draft);
        assert_eq!(parse_state("expired").unwrap(), JobState::Expired);

        let error = parse_state("bogus").unwrap_err();
        for state in JobState::ALL {
            assert!(error.contains(state.as_str()), "{error}");
        }
    }
}
