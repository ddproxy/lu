//! Ting: the node agent, with a scoped MCP server for job harnesses.

#[path = "../agent.rs"]
mod agent;
#[path = "../logging.rs"]
mod logging;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ting", version, about = "Ting: node agent for Lu")]
struct Cli {
    /// Config file (TOML).
    #[arg(long, global = true, default_value = "ting.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the scoped MCP protocol over stdio for a job harness.
    Mcp,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Mcp) => tuishou_ting::mcp::run().await,
        None => match agent::run(&cli.config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("ting: {error}");
                ExitCode::FAILURE
            }
        },
    }
}
