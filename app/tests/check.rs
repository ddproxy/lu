use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lu"))
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lu-check-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_token(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, "secret").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    p.display().to_string()
}

#[test]
fn check_controller_only() {
    let dir = scratch("controller");
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[controller]
listen = "127.0.0.1:7600"
db = "/var/lib/lu/lu.db"
artifacts_dir = "/var/lib/lu/artifacts"

[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"

[[controller.agents]]
name = "peer-b"
token_sha256 = "{HASH_B}"

[[controller.operators]]
name = "ops"
token_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#
        ),
    )
    .unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        "controller: listen 127.0.0.1:7600, 2 agent(s), 1 operator(s), tls off\nchecks: config values"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_agent_only() {
    let dir = scratch("agent");
    let token = write_token(&dir, "agent.token");
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[agent]
name = "peer-a"
controller_url = "https://lu.example.net:7600"
token_file = "{token}"
work_dir = "/srv/work/lu"
harness = "opencode"
heartbeat_secs = 15
"#
        ),
    )
    .unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        "agent: peer-a -> https://lu.example.net:7600, work dir /srv/work/lu\nchecks: config values"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_combined() {
    let dir = scratch("combined");
    let token = write_token(&dir, "agent.token");
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[controller]
listen = "127.0.0.1:7600"
db = "/var/lib/lu/lu.db"
artifacts_dir = "/var/lib/lu/artifacts"

[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"

[[controller.operators]]
name = "ops"
token_sha256 = "{HASH_B}"

[agent]
name = "peer-a"
controller_url = "https://lu.example.net:7600"
token_file = "{token}"
work_dir = "/srv/work/lu"
harness = "opencode"
heartbeat_secs = 15
"#
        ),
    )
    .unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(
        lines,
        vec![
            "controller: listen 127.0.0.1:7600, 1 agent(s), 1 operator(s), tls off",
            "agent: peer-a -> https://lu.example.net:7600, work dir /srv/work/lu",
            "checks: config values",
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_bad_file_exits_nonzero() {
    let dir = scratch("bad");
    let cfg = dir.join("bad.toml");
    std::fs::write(&cfg, "[nope]\n").unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("lu:"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_rejects_invalid_config_values() {
    let dir = scratch("invalid-values");
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[log]
level = "loud"

[controller]
listen = "127.0.0.1:7600"
db = "{db}"
artifacts_dir = "{artifacts}"

[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"
"#,
            db = dir.join("lu.db").display(),
            artifacts = dir.join("artifacts").display(),
        ),
    )
    .unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("level must be one of"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_rejects_an_invalid_scrub_pattern() {
    let dir = scratch("scrub-pattern");
    let token = write_token(&dir, "agent.token");
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[agent]
name = "peer-a"
controller_url = "https://lu.example.net:7600"
token_file = "{token}"
work_dir = "/srv/work/lu"
harness = "opencode"

[artifacts]
scrub = true

[[artifacts.extra_rules]]
name = "broken"
pattern = "(["
"#
        ),
    )
    .unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("[artifacts] config"), "{stderr}");
    assert!(stderr.contains("broken"), "{stderr}");
    assert!(stderr.contains("invalid pattern"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_reports_database_checks_when_the_database_exists() {
    let dir = scratch("database");
    let db = dir.join("lu.db");
    lu_store::sqlite::SqliteStore::open(&db).unwrap();
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[controller]
listen = "127.0.0.1:7600"
db = "{db}"
artifacts_dir = "{artifacts}"

[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"
"#,
            db = db.display(),
            artifacts = dir.join("artifacts").display(),
        ),
    )
    .unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("database: quick_check ok"), "{stdout}");
    assert!(
        stdout.contains("database: jobs without lease 0, orphan job_deps 0"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "checks: config values, database quick_check, database schema, database size, database leases, database job_deps"
        ),
        "{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_opens_no_secret_files() {
    let dir = scratch("secrets");
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let token = dir.join("agent.token");
    for path in [&cert, &key, &token] {
        std::fs::create_dir(path).unwrap();
    }
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[tls]
cert = "{cert}"
key = "{key}"

[controller]
listen = "127.0.0.1:7600"
db = "{db}"
artifacts_dir = "{artifacts}"

[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"

[agent]
name = "peer-a"
controller_url = "https://lu.example.net:7600"
token_file = "{token}"
work_dir = "/srv/work/lu"
harness = "opencode"
"#,
            cert = cert.display(),
            key = key.display(),
            db = dir.join("lu.db").display(),
            artifacts = dir.join("artifacts").display(),
            token = token.display(),
        ),
    )
    .unwrap();
    let out = bin()
        .args(["--config", cfg.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tls on"), "{stdout}");
    assert!(stdout.contains("checks: config values"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}
