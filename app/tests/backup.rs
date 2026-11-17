use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lu"))
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-backup-cli-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(dir: &Path, db: &Path) -> PathBuf {
    let path = dir.join("lu.toml");
    std::fs::write(
        &path,
        format!(
            "[controller]\nlisten = \"127.0.0.1:1\"\ndb = \"{}\"\nartifacts_dir = \"{}\"\n",
            db.display(),
            dir.join("artifacts").display()
        ),
    )
    .unwrap();
    path
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn backup_check_and_restore_round_trip() {
    let dir = scratch("round-trip");
    let db = dir.join("lu.db");
    let backup = dir.join("copy.db");
    {
        let _store = lu_store::sqlite::SqliteStore::open(&db).unwrap();
    }
    let cfg = config(&dir, &db);
    let cfg = cfg.to_str().unwrap();

    let output = run(&["--config", cfg, "backup", "--out", backup.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("backup: "), "{text}");
    assert!(text.contains("schema 2"), "{text}");
    assert!(text.contains("0 job(s)"), "{text}");
    assert_eq!(
        std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let output = run(&["--config", cfg, "check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("database: quick_check ok, schema 2, size "),
        "{text}"
    );
    assert!(
        text.contains("database: jobs without lease 0, orphan job_deps 0"),
        "{text}"
    );

    let output = run(&["--config", cfg, "restore", backup.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("restore: "), "{text}");
    assert!(text.contains("previous database: "), "{text}");
    let previous = text
        .lines()
        .find_map(|line| line.strip_prefix("previous database: "))
        .unwrap();
    assert_ne!(previous, "-");
    assert!(Path::new(previous).exists(), "{previous}");
    assert!(previous.contains(".pre-restore-"), "{previous}");

    let restored = lu_store::sqlite::SqliteStore::open(&db).unwrap();
    drop(restored);
    let _ = std::fs::remove_dir_all(&dir);
}
