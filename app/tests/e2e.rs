//! The whole path with real parts: the `lu serve` binary over a SQLite file, a client submitting
//! a job, the agent loop checking out a git repository, a scripted harness editing a file, and
//! the diff coming back as an artifact.

use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lu_client::{Client, Controller};
use lu_proto::{ArtifactName, JobSpec, JobState};
use sha2::{Digest, Sha256};
use tuishou_ting::Agent;

const OPERATOR_TOKEN: &str = "e2e-operator-token";
const AGENT_TOKEN: &str = "e2e-agent-token";

// `free_port` releases its listener before the controller binds it. Serialising the tests keeps
// two e2e runs from picking the same port and talking to each other's controller.
static E2E_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn sha(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "--local", "user.email", "e2e@example.net"]);
    git(dir, &["config", "--local", "user.name", "e2e"]);
    git(dir, &["config", "--local", "commit.gpgsign", "false"]);
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spec(repo: &Path, task: &str) -> JobSpec {
    serde_json::from_value(serde_json::json!({
        "source": {
            "repo": repo.to_str().unwrap(),
            "ref": "main",
        },
        "task": task,
        "timeout_secs": 60,
    }))
    .unwrap()
}

async fn wait_for_workspace_gone(work: &Path, id: &lu_proto::JobId) {
    let path = work.join(id.as_str());
    let deadline = Instant::now() + Duration::from_secs(60);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "workspace was not deleted: {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for(client: &Client, id: &lu_proto::JobId, want: JobState) -> lu_client::JobView {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let job = client.get_job(id).await.unwrap();
        if job.state == want {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "job stuck in {:?}, wanted {want:?}: {job:?}",
            job.state
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_runs_from_submit_to_diff() {
    let _serial = E2E_SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // A repository with one file.
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    std::fs::write(repo.join("hello.txt"), "hello\n").unwrap();
    git(&repo, &["add", "hello.txt"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    // The controller, as the real binary.
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let artifacts = root.join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    let config = root.join("lu.toml");
    std::fs::write(
        &config,
        format!(
            "[controller]\nlisten = \"127.0.0.1:{port}\"\ndb = \"{db}\"\nartifacts_dir = \"{art}\"\n\
             lease_secs = 30\n\
             [[controller.agents]]\nname = \"node-a\"\ntoken_sha256 = \"{ag}\"\n\
             [[controller.operators]]\nname = \"ops\"\ntoken_sha256 = \"{op}\"\n\
             [log]\nlevel = \"warn\"\nformat = \"text\"\nstderr = true\n",
            db = root.join("lu.db").display(),
            art = artifacts.display(),
            ag = sha(AGENT_TOKEN),
            op = sha(OPERATOR_TOKEN),
        ),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_lu"))
            .args(["serve", "--config"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );

    let operator = Client::new(&url, OPERATOR_TOKEN, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while operator.nodes().await.is_err() {
        assert!(Instant::now() < deadline, "controller did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let created = operator
        .submit(&spec(&repo, "append a line to hello.txt"))
        .await
        .unwrap();

    let mut dependent_spec = spec(&repo, "append a second line");
    dependent_spec.after = vec![created.id.clone()];
    let dependent = operator.submit(&dependent_spec).await.unwrap();
    assert_eq!(dependent.state, JobState::Blocked);
    assert_eq!(dependent.after, vec![created.id.clone()]);
    assert_eq!(dependent.waiting_on, vec![created.id.clone()]);

    // The agent, with a scripted harness standing in for opencode.
    let work = root.join("work");
    let token_file = root.join("agent.token");
    let mut file = std::fs::File::create(&token_file).unwrap();
    file.write_all(AGENT_TOKEN.as_bytes()).unwrap();
    drop(file);
    let agent_config = lu_config::Config::parse(&format!(
        "[agent]\nname = \"node-a\"\ncontroller_url = \"{url}\"\ntoken_file = \"{tf}\"\n\
         work_dir = \"{work}\"\nharness = \"opencode\"\nworkspace_group = \"{gid}\"\n",
        tf = token_file.display(),
        work = work.display(),
        gid = std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(root).unwrap()),
    ))
    .unwrap()
    .agent
    .unwrap();
    let controller: Arc<dyn Controller> = Arc::new(Client::new(&url, AGENT_TOKEN, None).unwrap());
    let agent = Agent::new(agent_config, controller)
        .unwrap()
        .with_command_builder(|_invocation| {
            Ok(lu_supervise::CommandSpec::new("sh")
                .arg("-c")
                .arg("echo working; echo 'added by the harness' >> hello.txt; echo done"))
        });
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = stop_rx.await;
            })
            .await
    });

    let job = wait_for(&operator, &created.id, JobState::Done).await;
    assert_eq!(job.attempt, 1);
    assert!(job.failure_class.is_none(), "{job:?}");

    let dependent = wait_for(&operator, &dependent.id, JobState::Done).await;
    assert_eq!(dependent.attempt, 1);
    assert!(dependent.failure_class.is_none(), "{dependent:?}");
    assert!(
        dependent.started_at.unwrap() >= job.finished_at.unwrap(),
        "dependent started before its dependency finished: {dependent:?} {job:?}"
    );
    assert!(dependent.waiting_on.is_empty(), "{dependent:?}");

    let diff = operator
        .fetch_artifact(&created.id, ArtifactName::DiffPatch)
        .await
        .unwrap();
    let diff = String::from_utf8(diff).unwrap();
    assert!(diff.contains("+added by the harness"), "{diff}");

    let log = operator
        .fetch_artifact(&created.id, ArtifactName::RunLog)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&log).contains("working"));

    // The workspaces are gone after controller release and the agent ends cleanly on shutdown.
    wait_for_workspace_gone(&work, &created.id).await;
    wait_for_workspace_gone(&work, &dependent.id).await;
    let _ = stop_tx.send(());
    let ended = tokio::time::timeout(Duration::from_secs(30), handle).await;
    assert!(ended.is_ok(), "agent did not stop");
}

fn controller_config(root: &Path, port: u16, audit_path: Option<&Path>) -> String {
    let mut text = format!(
        "[controller]\nlisten = \"127.0.0.1:{port}\"\ndb = \"{}\"\nartifacts_dir = \"{}\"\n\
         lease_secs = 30\n\
         [[controller.agents]]\nname = \"node-a\"\ntoken_sha256 = \"{}\"\n\
         [[controller.operators]]\nname = \"ops\"\ntoken_sha256 = \"{}\"\n\
         [log]\nlevel = \"warn\"\nformat = \"text\"\nstderr = true\n",
        root.join("lu.db").display(),
        root.join("artifacts").display(),
        sha(AGENT_TOKEN),
        sha(OPERATOR_TOKEN),
    );
    if let Some(path) = audit_path {
        text.push_str(&format!("\n[audit]\npath = \"{}\"\n", path.display()));
    }
    text
}

fn spawn_controller(root: &Path, port: u16, audit_path: Option<&Path>) -> Server {
    let config = root.join("lu.toml");
    std::fs::create_dir_all(root.join("artifacts")).unwrap();
    std::fs::write(&config, controller_config(root, port, audit_path)).unwrap();
    Server(
        Command::new(env!("CARGO_BIN_EXE_lu"))
            .args(["serve", "--config"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

async fn ready(url: &str) -> Client {
    let client = Client::new(url, OPERATOR_TOKEN, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while client.nodes().await.is_err() {
        assert!(Instant::now() < deadline, "controller did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    client
}

async fn submit(client: &Client, task: &str) {
    let secret_job = serde_json::from_value(serde_json::json!({
        "source": {"repo": "https://example.net/team/repo.git", "ref": "main"},
        "task": task,
    }))
    .unwrap();
    client.submit(&secret_job).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_record_is_off_by_default_and_continues_across_restart() {
    use lu_audit::verify_file;

    let _serial = E2E_SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let audit_path = root.join("audit.jsonl");
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let server = spawn_controller(root, port, Some(&audit_path));
    let operator = ready(&url).await;
    submit(&operator, "task text must never be audited").await;
    submit(&operator, "another secret task").await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(outcome) = verify_file(&audit_path, None) {
            if outcome.records >= 2 {
                break;
            }
        }
        assert!(Instant::now() < deadline, "audit records not written");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(server);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let first = verify_file(&audit_path, None).unwrap();
    let bytes = std::fs::read(&audit_path).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.contains("\"event\":\"submitted\""));
    assert!(text.contains("\"actor\":\"ops\""));
    for secret in ["must never be audited", "another secret task", "task"] {
        assert!(
            !text.contains(secret),
            "task text leaked into the audit file"
        );
    }
    // A trusted head verifies a sealed file: its final hash must still be the anchored one.
    lu_audit::verify_bytes(&bytes, Some(&first.head)).unwrap();

    // A fresh controller process resumes the hash chain instead of starting a new one.
    let server = spawn_controller(root, port, Some(&audit_path));
    let operator = ready(&url).await;
    submit(&operator, "third task").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(outcome) = verify_file(&audit_path, None) {
            if outcome.records > first.records {
                break;
            }
        }
        assert!(Instant::now() < deadline, "audit did not continue");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let second = verify_file(&audit_path, None).unwrap();
    assert!(second.records > first.records);
    assert_ne!(second.head, first.head);
    let continued = std::fs::read(&audit_path).unwrap();
    let continued = String::from_utf8(continued).unwrap();
    // The old head is still present as an earlier line: the prefix was not rewritten.
    assert!(continued.contains(&format!("\"hash\":\"{}\"", first.head)));
    assert!(!continued.contains("third task"));
    drop(server);

    // Without [audit], no record file is created at all.
    let other = tempfile::tempdir().unwrap();
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let server = spawn_controller(other.path(), port, None);
    let operator = ready(&url).await;
    submit(&operator, "do not record this").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!other.path().join("audit.jsonl").exists());
    drop(server);
}
