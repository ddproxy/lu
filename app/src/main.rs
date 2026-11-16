//! lu: a job queue and dispatcher for dev peers.

mod logging;
mod operator;
mod service;
#[cfg(feature = "tui")]
mod top;

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
    /// Parse and validate the config and the TLS files, print a summary, and exit.
    Check,
    /// Run the HTTP listener. This is the default.
    Serve,
    /// Live terminal dashboard demo. Needs a build with the `tui` feature. Ignores --config.
    #[cfg(feature = "tui")]
    Top,
    /// Submit a job.
    Submit(operator::SubmitArgs),
    /// List jobs.
    Ls(operator::LsArgs),
    /// Show one job.
    Show(operator::ShowArgs),
    /// Print a job's log.
    Logs(operator::LogsArgs),
    /// Fetch the three artifacts of a job.
    Fetch(operator::FetchArgs),
    /// Ask the controller to cancel a job.
    Cancel(operator::CancelArgs),
    /// List nodes.
    Nodes(operator::NodesArgs),
    /// Stop assigning new jobs to a node.
    Drain(operator::NodeArgs),
    /// Allow a drained node to take jobs again.
    Undrain(operator::NodeArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Check => check(&cli.config),
        Command::Serve => serve(&cli.config).await,
        #[cfg(feature = "tui")]
        Command::Top => match top::run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        },
        Command::Submit(args) => operator::submit(args, &cli.config, json).await,
        Command::Ls(args) => operator::ls(args, &cli.config, json).await,
        Command::Show(args) => operator::show(args, &cli.config, json).await,
        Command::Logs(args) => operator::logs(args, &cli.config, json).await,
        Command::Fetch(args) => operator::fetch(args, &cli.config, json).await,
        Command::Cancel(args) => operator::cancel(args, &cli.config, json).await,
        Command::Nodes(args) => operator::nodes(args, &cli.config, json).await,
        Command::Drain(args) => operator::drain(args, &cli.config, json).await,
        Command::Undrain(args) => operator::undrain(args, &cli.config, json).await,
    }
}

fn check(path: &std::path::Path) -> ExitCode {
    let config = match lu_config::load(path) {
        Ok(config) => config,
        Err(error) => return fail(&error.to_string()),
    };
    if let Some(tls) = &config.tls {
        if let Err(error) = lu_serve::tls::load(
            std::path::Path::new(&tls.cert),
            std::path::Path::new(&tls.key),
        ) {
            return fail(&error);
        }
    }
    let tls_on = config.tls.is_some();
    if let Some(c) = &config.controller {
        println!(
            "controller: listen {}, {} agent(s), {} operator(s), tls {}",
            c.listen,
            c.agents.len(),
            c.operators.len(),
            if tls_on { "on" } else { "off" }
        );
    }
    if let Some(a) = &config.agent {
        println!(
            "agent: {} -> {}, work dir {}, slots {}",
            a.name, a.controller_url, a.work_dir, a.slots
        );
    }
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
