//! The whole path with real parts: the `lu serve` binary over a SQLite file, a client submitting
//! a job, the agent loop checking out a git repository, a scripted harness editing a file, and
//! the diff coming back as an artifact.

use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lu_agent::Agent;
use lu_client::{Client, Controller};
use lu_proto::{ArtifactName, JobSpec, JobState};
use sha2::{Digest, Sha256};

const OPERATOR_TOKEN: &str = "e2e-operator-token";
const AGENT_TOKEN: &str = "e2e-agent-token";

fn sha(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.email=e2e@example.net", "-c", "user.name=e2e"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
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
        "repo": repo.to_str().unwrap(),
        "git_ref": "main",
        "task": task,
        "timeout_secs": 60,
    }))
    .unwrap()
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
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // A repository with one file.
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
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

    // The agent, with a scripted harness standing in for opencode.
    let work = root.join("work");
    let token_file = root.join("agent.token");
    let mut file = std::fs::File::create(&token_file).unwrap();
    file.write_all(AGENT_TOKEN.as_bytes()).unwrap();
    drop(file);
    let agent_config = lu_config::Config::parse(&format!(
        "[agent]\nname = \"node-a\"\ncontroller_url = \"{url}\"\ntoken_file = \"{tf}\"\n\
         work_dir = \"{work}\"\nharness = \"opencode\"\n",
        tf = token_file.display(),
        work = work.display(),
    ))
    .unwrap()
    .agent
    .unwrap();
    let controller: Arc<dyn Controller> = Arc::new(Client::new(&url, AGENT_TOKEN, None).unwrap());
    let agent = Agent::new(agent_config, controller)
        .unwrap()
        .with_command_builder(|_task_file, _model, _server, _message| {
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

    // The workspace is gone and the agent ends cleanly on shutdown.
    assert!(!work.join(created.id.as_str()).exists());
    let _ = stop_tx.send(());
    let ended = tokio::time::timeout(Duration::from_secs(30), handle).await;
    assert!(ended.is_ok(), "agent did not stop");
}
