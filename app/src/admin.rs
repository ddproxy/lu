//! Local database commands. They read the controller database path from the config and never
//! contact a running controller; restore refuses while the database is in use.

use std::path::{Path, PathBuf};

#[derive(Debug, clap::Args)]
pub struct BackupArgs {
    /// File to write the live database copy to.
    #[arg(long)]
    pub out: PathBuf,
    /// Replace an existing backup file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, clap::Args)]
pub struct RestoreArgs {
    /// Backup file to install as the controller database.
    pub file: PathBuf,
}

pub fn backup(args: BackupArgs, config: &Path) -> Result<(), String> {
    let db = controller_db(config)?;
    let info = lu_store::backup::backup(&db, &args.out, args.force)?;
    println!(
        "backup: {} ({} bytes, schema {}, {} job(s))",
        args.out.display(),
        info.bytes,
        info.schema_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        info.jobs
    );
    Ok(())
}

pub fn restore(args: RestoreArgs, config: &Path) -> Result<(), String> {
    let db = controller_db(config)?;
    let info = lu_store::backup::restore(&args.file, &db)?;
    println!("restore: {} -> {}", args.file.display(), db.display());
    match info.pre_restore {
        Some(path) => println!("previous database: {}", path.display()),
        None => println!("previous database: -"),
    }
    Ok(())
}

/// Reads the configured database, when it exists, and returns the lines `lu check` prints.
pub fn database_check(config: &Path) -> Result<Vec<String>, String> {
    let db = controller_db(config)?;
    if !db.exists() {
        return Ok(Vec::new());
    }
    let check = lu_store::backup::inspect(&db)?;
    let schema = check
        .schema_version
        .map(|version| version.to_string())
        .unwrap_or_else(|| "-".to_owned());
    Ok(vec![
        format!(
            "database: quick_check {}, schema {}, size {} bytes",
            check.quick_check, schema, check.bytes
        ),
        format!(
            "database: jobs without lease {}, orphan job_deps {}",
            check.leased_without_lease, check.orphan_job_deps
        ),
    ])
}

fn controller_db(config: &Path) -> Result<PathBuf, String> {
    lu_config::controller_db(config).map_err(|error| error.to_string())
}
