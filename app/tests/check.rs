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
        "controller: listen 127.0.0.1:7600, 2 agent(s), 1 operator(s), tls off"
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
slots = 1
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
        "agent: peer-a -> https://lu.example.net:7600, work dir /srv/work/lu, slots 1"
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
slots = 1
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
            "agent: peer-a -> https://lu.example.net:7600, work dir /srv/work/lu, slots 1",
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
