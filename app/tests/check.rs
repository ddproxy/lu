use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
fn check_reports_intake_bounds_and_path_errors() {
    let dir = scratch("validation-bugs");
    let port = free_port();
    let base = controller_text(&dir, port);
    let intake = |field: &str, value: &str| {
        format!(
            "{base}\n[intake]\n[[intake.sources]]\nname = \"events\"\nsecret_file = \"/etc/lu/events.key\"\n[[intake.sources.rules]]\nrepo = \"https://example.net/a.git\"\nref = \"refs/heads/main\"\n{field} = {value}\n"
        )
    };
    let agent = |extra: &str| {
        format!(
            "[agent]\nname = \"peer-a\"\ncontroller_url = \"https://lu.example.net:7600\"\ntoken_file = \"/etc/ting/ting.token\"\nwork_dir = \"/srv/work/lu\"\nharness = \"opencode\"\n{extra}"
        )
    };
    let cases = [
        (
            "priority",
            intake("priority", "10"),
            "priority must be 0 to 9",
        ),
        (
            "timeout",
            intake("timeout", "0"),
            "timeout must be 1 to 86400 seconds",
        ),
        (
            "log-file",
            format!("[log]\nfile = \"relative.log\"\n\n{base}"),
            "file must be an absolute path",
        ),
        (
            "tls-cert",
            format!("{base}\n[tls]\ncert = \"relative.pem\"\nkey = \"/etc/lu/key.pem\"\n"),
            "cert must be an absolute path",
        ),
        (
            "intake-secret",
            format!(
                "{base}\n[intake]\n[[intake.sources]]\nname = \"events\"\nsecret_file = \"/etc/lu/../events.key\"\n[[intake.sources.rules]]\nrepo = \"https://example.net/a.git\"\nref = \"refs/heads/main\"\n"
            ),
            "secret_file must not contain '..'",
        ),
        (
            "repo-credential",
            format!(
                "{base}\n[[controller.repos]]\nname = \"mirror\"\nurl = \"https://example.net/a.git\"\ncredential_file = \"relative.credential\"\n"
            ),
            "credential_file must be an absolute path",
        ),
        (
            "mirror-token",
            format!(
                "{base}\n[mirror.credentials]\n[[mirror.credentials.repos]]\nname = \"mirror\"\ntoken_file = \"relative.token\"\n"
            ),
            "token_file must be an absolute path",
        ),
        (
            "agent-token",
            "[agent]\nname = \"peer-a\"\ncontroller_url = \"https://lu.example.net:7600\"\ntoken_file = \"relative.token\"\nwork_dir = \"/srv/work/lu\"\nharness = \"opencode\"\n".to_owned(),
            "token_file must be an absolute path",
        ),
        (
            "agent-ca",
            agent("ca_file = \"/etc/lu/../ca.pem\"\n"),
            "ca_file must not contain '..'",
        ),
        (
            "agent-profile-server",
            agent("[agent.harness_profiles.opencode]\nserver = \"ftp://example.net\"\n"),
            "server must be an http:// or https:// URL",
        ),
        (
            "gateway-token",
            agent("provider_gateway_health_url = \"http://127.0.0.1:4100/v1/health\"\nprovider_gateway_token_file = \"relative.token\"\n"),
            "provider_gateway_token_file must be an absolute path",
        ),
        (
            "controller-url",
            "[agent]\nname = \"peer-a\"\ncontroller_url = \"https://lu.example.net/a/../b\"\ntoken_file = \"/etc/ting/ting.token\"\nwork_dir = \"/srv/work/lu\"\nharness = \"opencode\"\n".to_owned(),
            "controller_url path must not contain . or .. segments",
        ),
        (
            "file-url",
            format!(
                "{base}\n[[controller.repos]]\nname = \"mirror\"\nurl = \"file:///srv/../repo.git\"\nallow_file_urls = true\n"
            ),
            "file url must not contain '..'",
        ),
    ];
    for (label, text, expected) in cases {
        let case_dir = dir.join(label);
        std::fs::create_dir_all(&case_dir).unwrap();
        let config = case_dir.join("c.toml");
        std::fs::write(&config, text).unwrap();
        let out = bin()
            .args(["--config", config.to_str().unwrap(), "check"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{label}: check passed");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{label}: {stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_accepts_only_canonical_schedule_tables() {
    let dir = scratch("schedule-tables");
    let base = controller_text(&dir, free_port());
    let canonical = format!(
        "{base}\n[[calendar]]\nname = \"office\"\n[[attach]]\nscope = \"global\"\ncalendars = [\"office\"]\n"
    );
    let good = dir.join("canonical.toml");
    std::fs::write(&good, canonical).unwrap();
    let out = bin()
        .args(["--config", good.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "canonical tables refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cases = [
        (
            "calendars",
            "[[calendars]]\nname = \"office\"\n",
            "use [[calendar]]",
        ),
        (
            "attachments",
            "[[attachments]]\nscope = \"global\"\ncalendars = []\n",
            "use [[attach]]",
        ),
        (
            "nested-calendars",
            "[schedule]\n[[schedule.calendars]]\nname = \"office\"\n",
            "use [[calendar]]",
        ),
        (
            "nested-attachments",
            "[schedule]\n[[schedule.attachments]]\nscope = \"global\"\ncalendars = []\n",
            "use [[attach]]",
        ),
        (
            "dates",
            "[[calendar]]\nname = \"office\"\ndates = []\n",
            "use ranges",
        ),
    ];
    for (label, extra, expected) in cases {
        let case_dir = dir.join(label);
        std::fs::create_dir_all(&case_dir).unwrap();
        let config = case_dir.join("c.toml");
        std::fs::write(&config, format!("{base}\n{extra}")).unwrap();
        let out = bin()
            .args(["--config", config.to_str().unwrap(), "check"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{label}: check passed");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{label}: {stderr}");
    }
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

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn controller_text(dir: &Path, listen: u16) -> String {
    format!(
        r#"[controller]
listen = "127.0.0.1:{listen}"
db = "{db}"
artifacts_dir = "{artifacts}"

[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"

[[controller.operators]]
name = "ops"
token_sha256 = "{HASH_B}"
"#,
        db = dir.join("lu.db").display(),
        artifacts = dir.join("artifacts").display(),
    )
}

/// Starts `lu serve` long enough to prove it passed startup validation, then stops it.
fn serve_starts(config: &Path, listen: u16) -> Result<(), String> {
    let mut child = bin()
        .args(["--config", config.to_str().unwrap(), "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            return Err(format!(
                "serve exited before binding: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        if TcpStream::connect(("127.0.0.1", listen)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    if ready {
        Ok(())
    } else {
        Err(format!(
            "serve did not bind: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

#[test]
fn check_and_serve_agree_on_config_validation() {
    let dir = scratch("agreement");
    let listen = free_port();
    let case_dir = dir.join("valid");
    std::fs::create_dir_all(&case_dir).unwrap();
    let good = case_dir.join("c.toml");
    std::fs::write(&good, controller_text(&case_dir, listen)).unwrap();

    let checked = bin()
        .args(["--config", good.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(
        !case_dir.join("lu.db").exists(),
        "check must not create the database"
    );
    serve_starts(&good, listen).expect("valid config should serve");

    let base = controller_text(&case_dir, listen);
    let cases = [
        ("unknown-key", "[nope]\nvalue = 1\n".to_owned(), "unknown field"),
        (
            "log-level",
            format!("[log]\nlevel = \"loud\"\n\n{base}"),
            "level must be one of",
        ),
        (
            "token-scope",
            format!(
                "{base}\n[[controller.operators]]\nname = \"ops2\"\ntoken_sha256 = \"{HASH_A}\"\n"
            ),
            "a token must not work in both scopes",
        ),
        (
            "scrub-pattern",
            format!(
                "{base}\n[artifacts]\nscrub = true\n\n[[artifacts.extra_rules]]\nname = \"broken\"\npattern = \"([\"\n"
            ),
            "invalid pattern",
        ),
        (
            "schedule",
            format!("{base}\n[schedule]\nhorizon_secs = 0\n"),
            "[schedule]",
        ),
        (
            "db-path",
            base.replace(
                &format!("db = \"{}\"", case_dir.join("lu.db").display()),
                "db = \"relative.db\"",
            ),
            "db must be an absolute path",
        ),
    ];
    for (label, text, expected) in cases {
        let case_dir = dir.join(label);
        std::fs::create_dir_all(&case_dir).unwrap();
        let config = case_dir.join("c.toml");
        std::fs::write(&config, text).unwrap();

        let checked = bin()
            .args(["--config", config.to_str().unwrap(), "check"])
            .output()
            .unwrap();
        let served = bin()
            .args(["--config", config.to_str().unwrap(), "serve"])
            .output()
            .unwrap();
        let check_stderr = String::from_utf8_lossy(&checked.stderr).into_owned();
        let serve_stderr = String::from_utf8_lossy(&served.stderr).into_owned();
        assert!(
            !checked.status.success(),
            "{label}: check passed: {check_stderr}"
        );
        assert!(
            !served.status.success(),
            "{label}: serve passed: {serve_stderr}"
        );
        assert_eq!(check_stderr, serve_stderr, "{label}: messages differ");
        assert!(check_stderr.contains(expected), "{label}: {check_stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
