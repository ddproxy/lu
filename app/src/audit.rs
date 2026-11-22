//! `lu audit`: inspect the append-only, hash-chained job event record.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct AuditArgs {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Walk the chain and report the first break.
    Verify {
        /// Audit JSON Lines file.
        path: PathBuf,
        /// Trusted final head recorded elsewhere; checks a sealed or rotated file.
        #[arg(long)]
        head: Option<String>,
    },
    /// Print the last records.
    Tail {
        /// Audit JSON Lines file.
        path: PathBuf,
        /// Number of records to print.
        #[arg(short, long, default_value_t = 20)]
        lines: usize,
    },
    /// Print the current head hash, for anchoring elsewhere.
    Head {
        /// Audit JSON Lines file.
        path: PathBuf,
    },
}

pub fn run(args: AuditArgs, json: bool) -> Result<(), String> {
    match args.command {
        Command::Verify { path, head } => {
            let outcome = lu_audit::verify_file(&path, head.as_deref())
                .map_err(|error| format!("{}: {error}", path.display()))?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "records": outcome.records,
                        "head": outcome.head,
                    })
                );
            } else {
                println!("ok: {} record(s), head {}", outcome.records, outcome.head);
            }
        }
        Command::Tail { path, lines } => {
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            for line in lu_audit::tail_bytes(&bytes, lines) {
                println!("{line}");
            }
        }
        Command::Head { path } => {
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let outcome = lu_audit::head_bytes(&bytes)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "head": outcome.head,
                        "records": outcome.records,
                    })
                );
            } else {
                println!("{}", outcome.head);
            }
        }
    }
    Ok(())
}
