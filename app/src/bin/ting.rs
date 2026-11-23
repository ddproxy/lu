//! Ting: the node agent, with a scoped MCP server for job harnesses and a small set of
//! self-service maintenance commands driven by the node's own token.

#[path = "../agent.rs"]
mod agent;
#[path = "../logging.rs"]
mod logging;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use lu_client::Client;

/// Where the node's config lives unless `--config` says otherwise.
pub const DEFAULT_CONFIG: &str = "/etc/ting/ting.toml";
/// How often `wait-idle` asks the controller for the node's own state.
const WAIT_POLL: Duration = Duration::from_secs(1);
const EXIT_ERROR: u8 = 1;
const EXIT_CONFIG: u8 = 2;
const EXIT_TIMEOUT: u8 = 3;

#[derive(Parser)]
#[command(name = "ting", version, about = "Ting: node agent for Lu")]
struct Cli {
    /// Config file (TOML), default /etc/ting/ting.toml.
    #[arg(long, global = true, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the scoped MCP protocol over stdio for a job harness.
    Mcp,
    /// Stop assigning new jobs to this node. The current job keeps running.
    Drain,
    /// Allow this node to take jobs again after a drain it started itself.
    Undrain,
    /// Show this node's own drain and job state.
    Status {
        /// Print one JSON object with exactly the fields the controller returns.
        #[arg(long)]
        json: bool,
    },
    /// Wait until this node is drained and has no current job.
    WaitIdle {
        /// Maximum time to wait: seconds, or a value with an s/m/h suffix.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        timeout: Duration,
    },
}

enum CliError {
    Config(String),
    Runtime(String),
    Timeout,
}

impl CliError {
    fn code(&self) -> u8 {
        match self {
            Self::Config(_) => EXIT_CONFIG,
            Self::Runtime(_) => EXIT_ERROR,
            Self::Timeout => EXIT_TIMEOUT,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Config(message) | Self::Runtime(message) => message,
            Self::Timeout => "timed out waiting for the node to become idle",
        }
    }
}

/// Accepts plain seconds or a single `s`, `m` or `h` suffix. Zero and overflow are usage errors.
fn parse_duration(value: &str) -> Result<Duration, String> {
    let (amount, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') | Some(b'S') => (&value[..value.len() - 1], 1u64),
        Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 60u64),
        Some(b'h') | Some(b'H') => (&value[..value.len() - 1], 3600u64),
        _ => (value, 1u64),
    };
    if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(
            "timeout must be a positive integer with an optional s, m or h suffix".to_owned(),
        );
    }
    let amount: u64 = amount
        .parse()
        .map_err(|_| "timeout value is too large".to_owned())?;
    if amount == 0 {
        return Err("timeout must be greater than zero".to_owned());
    }
    let secs = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "timeout value is too large".to_owned())?;
    Ok(Duration::from_secs(secs))
}

/// Reads the node's config and token, then builds a client bound to that token. Config and token
/// problems are usage/configuration failures (exit 2), not controller failures (exit 1).
fn agent_client(config_path: &Path) -> Result<Client, CliError> {
    let config = lu_config::load(config_path).map_err(|error| {
        CliError::Config(format!(
            "cannot read config {}: {error}",
            config_path.display()
        ))
    })?;
    let agent = config.agent.ok_or_else(|| {
        CliError::Config(format!(
            "config {} has no [agent] section",
            config_path.display()
        ))
    })?;
    let token = lu_config::read_private_file(
        "[agent] token_file",
        Path::new(&agent.token_file),
        lu_config::MAX_CONFIG_BYTES,
    )
    .map_err(|error| CliError::Config(format!("cannot read agent token file: {error}")))?;
    let token = token.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        return Err(CliError::Config("agent token file is empty".to_owned()));
    }
    let ca_file = agent.ca_file.as_ref().map(PathBuf::from);
    Client::new(&agent.controller_url, token, ca_file)
        .map_err(|error| CliError::Config(format!("invalid agent controller settings: {error}")))
}

fn controller_error(error: lu_client::ApiError) -> CliError {
    CliError::Runtime(error.to_string())
}

async fn wait_idle(client: &Client, timeout: Duration) -> Result<(), CliError> {
    let deadline = Instant::now() + timeout;
    loop {
        let view = client.agent_self().await.map_err(controller_error)?;
        if view.draining && view.current_job.is_none() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CliError::Timeout);
        }
        tokio::time::sleep(WAIT_POLL.min(remaining)).await;
    }
}

async fn run_command(command: Command, config_path: &Path) -> Result<(), CliError> {
    let client = agent_client(config_path)?;
    match command {
        Command::Drain => client.agent_drain().await.map_err(controller_error),
        Command::Undrain => client.agent_undrain().await.map_err(controller_error),
        Command::Status { json } => {
            let view = client.agent_self().await.map_err(controller_error)?;
            if json {
                let line = serde_json::to_string(&view)
                    .map_err(|error| CliError::Runtime(format!("cannot encode status: {error}")))?;
                println!("{line}");
            } else {
                let job = view
                    .current_job
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "-".to_owned());
                println!(
                    "{}: version={} draining={} current_job={} last_heartbeat_age_secs={} online={}",
                    view.name,
                    view.version.unwrap_or_else(|| "-".to_owned()),
                    view.draining,
                    job,
                    view.last_heartbeat_age_secs,
                    view.online
                );
            }
            Ok(())
        }
        Command::WaitIdle { timeout } => wait_idle(&client, timeout).await,
        Command::Mcp => unreachable!("mcp never loads a controller client"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.command {
        None => agent::run(&cli.config).await.map_err(CliError::Runtime),
        Some(Command::Mcp) => return tuishou_ting::mcp::run().await,
        Some(command) => run_command(command, &cli.config).await,
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ting: {}", error.message());
            ExitCode::from(error.code())
        }
    }
}
