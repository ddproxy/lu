//! lu: a job queue and dispatcher for dev peers.

mod admin;
mod audit;
mod batch;
mod logging;
mod mcp;
mod operator;
mod service;
mod templates;
#[cfg(feature = "tui")]
mod top;

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod mock;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lu", version, about = "Job queue and dispatcher for dev peers")]
struct Cli {
    /// Config file (TOML).
    #[arg(long, global = true, default_value = "lu.toml")]
    config: PathBuf,
    /// Print the raw API JSON instead of aligned text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print a config summary and database health without reading secret files.
    Check,
    /// Copy the controller database to a file while the controller keeps running.
    Backup(admin::BackupArgs),
    /// Install a backup as the controller database after stopping the controller.
    Restore(admin::RestoreArgs),
    /// Verify, tail or anchor the hash-chained audit record.
    Audit(audit::AuditArgs),
    /// Show controller uptime, database size, queue and node counts.
    Status(operator::StatusArgs),
    /// Report terminal-job contributions by node, repository or state.
    Stats(operator::StatsArgs),
    /// Serve the Model Context Protocol over stdio (read and write tools).
    Mcp,
    /// List the repositories the controller mirrors.
    Repos(operator::ReposArgs),
    /// Sync one repository mirror.
    Sync(operator::SyncArgs),
    /// Run the HTTP listener. This is the default.
    Serve,
    /// Live terminal view of the nodes and recent jobs. Needs a build with the `tui` feature.
    #[cfg(feature = "tui")]
    Top(top::TopArgs),
    /// Submit a job.
    Submit(Box<operator::SubmitArgs>),
    /// Create one collection job from earlier inputs (keys or ids).
    Collect(operator::CollectArgs),
    /// Resubmit a finished job's spec as a new job.
    Retry(operator::RetryArgs),
    /// Change the provider, model, fallbacks, labels, taints or dependencies of a queued or blocked job.
    Edit(operator::EditArgs),
    /// List jobs.
    Ls(operator::LsArgs),
    /// List stored drafts, oldest first.
    Drafts(operator::DraftsArgs),
    /// Approve a draft, optionally editing it first.
    Approve(operator::ApproveArgs),
    /// Reject and cancel a draft.
    Reject(operator::RejectArgs),
    /// Show one job.
    Show(operator::ShowArgs),
    /// Publish a finished job's diff to its configured target.
    Publish(operator::PublishArgs),
    /// Draw job dependency graphs.
    Graph(operator::GraphArgs),
    /// Print a job's log.
    Logs(operator::LogsArgs),
    /// Fetch the three artifacts of a job.
    Fetch(operator::FetchArgs),
    /// Ask the controller to cancel a job.
    Cancel(operator::CancelArgs),
    /// List nodes.
    Nodes(operator::NodesArgs),
    /// Show the host and job stats a node last reported.
    Node(operator::NodeArgs),
    /// Stop assigning new jobs to a node. The node must have heartbeated at least once.
    Drain(operator::NodeTargetArgs),
    /// Allow a drained node to take jobs again. The node must have heartbeated at least once.
    Undrain(operator::NodeTargetArgs),
    /// Clear provider cordons on a node, or just one provider's cordon.
    Uncordon(operator::UncordonArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Check => check(&cli.config),
        Command::Backup(args) => match admin::backup(args, &cli.config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        },
        Command::Restore(args) => match admin::restore(args, &cli.config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        },
        Command::Audit(args) => match audit::run(args, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        },
        Command::Status(args) => operator::status(args, &cli.config, json).await,
        Command::Stats(args) => operator::stats(args, &cli.config, json).await,
        Command::Mcp => mcp::run(&cli.config).await,
        Command::Repos(args) => operator::repos(args, &cli.config, json).await,
        Command::Sync(args) => operator::sync_repo(args, &cli.config, json).await,
        Command::Serve => serve(&cli.config).await,
        #[cfg(feature = "tui")]
        Command::Top(args) => match top::run(&cli.config, args, json).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        },
        Command::Submit(args) => operator::submit(*args, &cli.config, json).await,
        Command::Collect(args) => operator::collect(args, &cli.config, json).await,
        Command::Retry(args) => operator::retry(args, &cli.config, json).await,
        Command::Edit(args) => operator::edit(args, &cli.config, json).await,
        Command::Ls(args) => operator::ls(args, &cli.config, json).await,
        Command::Drafts(args) => operator::drafts(args, &cli.config, json).await,
        Command::Approve(args) => operator::approve(args, &cli.config, json).await,
        Command::Reject(args) => operator::reject(args, &cli.config, json).await,
        Command::Show(args) => operator::show(args, &cli.config, json).await,
        Command::Publish(args) => operator::publish(args, &cli.config, json).await,
        Command::Graph(args) => operator::graph(args, &cli.config, json).await,
        Command::Logs(args) => operator::logs(args, &cli.config, json).await,
        Command::Fetch(args) => operator::fetch(args, &cli.config, json).await,
        Command::Cancel(args) => operator::cancel(args, &cli.config, json).await,
        Command::Nodes(args) => operator::nodes(args, &cli.config, json).await,
        Command::Node(args) => operator::node(args, &cli.config, json).await,
        Command::Drain(args) => operator::drain(args, &cli.config, json).await,
        Command::Undrain(args) => operator::undrain(args, &cli.config, json).await,
        Command::Uncordon(args) => operator::uncordon(args, &cli.config, json).await,
    }
}

fn check(path: &std::path::Path) -> ExitCode {
    let config = match lu_config::load_values(path) {
        Ok(config) => config,
        Err(error) => return fail(&error.to_string()),
    };
    if let Err(error) = lu_controller::Scrubber::validate_config(config.artifacts.as_ref()) {
        return fail(&format!("invalid [artifacts] config: {error}"));
    }
    let mut checks = vec!["config values".to_owned()];
    let tls_on = config.tls.is_some();
    if let Some(c) = &config.controller {
        println!(
            "controller: listen {}, {} agent(s), {} operator(s), tls {}",
            c.listen,
            c.agents.len(),
            c.operators.len(),
            if tls_on { "on" } else { "off" }
        );
        match admin::database_check(path) {
            Ok(lines) => {
                if !lines.is_empty() {
                    checks.extend(
                        [
                            "database quick_check",
                            "database schema",
                            "database size",
                            "database leases",
                            "database job_deps",
                        ]
                        .iter()
                        .map(|name| (*name).to_owned()),
                    );
                }
                for line in lines {
                    println!("{line}");
                }
            }
            Err(error) => return fail(&error),
        }
    }
    if let Some(a) = &config.artifacts {
        println!(
            "artifacts: scrubbing {}, {} extra rule(s)",
            if a.scrub { "on" } else { "off" },
            a.extra_rules.len()
        );
    }
    if let Some(a) = &config.agent {
        println!(
            "agent: {} -> {}, work dir {}",
            a.name, a.controller_url, a.work_dir
        );
    }
    println!("checks: {}", checks.join(", "));
    ExitCode::SUCCESS
}

async fn serve(path: &std::path::Path) -> ExitCode {
    let config = match lu_config::load(path) {
        Ok(config) => config,
        Err(error) => return fail(&error.to_string()),
    };
    if let Err(error) = logging::init(&config.log) {
        return fail(&error);
    }
    match service::run(&config, path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "serve failed");
            fail(&error)
        }
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("lu: {message}");
    ExitCode::FAILURE
}
