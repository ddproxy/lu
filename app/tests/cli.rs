//! End-to-end CLI tests against an in-process fake controller. No real controller is needed.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use lu_proto::{ArtifactName, JobId};
use serde_json::{json, Value};
use tokio::net::TcpListener;

const TOKEN: &str = "cli-operator-token-do-not-print";

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn json(value: &Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: value.to_string().into_bytes(),
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

type Responder = dyn Fn(&Recorded) -> Reply + Send + Sync;

#[derive(Clone)]
struct FakeState {
    records: Arc<Mutex<Vec<Recorded>>>,
    responder: Arc<Responder>,
}

struct TestServer {
    url: String,
    records: Arc<Mutex<Vec<Recorded>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start<F>(responder: F) -> TestServer
where
    F: Fn(&Recorded) -> Reply + Send + Sync + 'static,
{
    let records = Arc::new(Mutex::new(Vec::new()));
    let state = FakeState {
        records: Arc::clone(&records),
        responder: Arc::new(responder),
    };
    let app = Router::new().fallback(fallback).with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TestServer {
        url: format!("http://{addr}"),
        records,
        handle,
    }
}

async fn fallback(
    State(state): State<FakeState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let recorded = Recorded {
        method: method.to_string(),
        path: uri.path().to_owned(),
        query: uri.query().map(str::to_owned),
        headers: headers
            .iter()
            .filter_map(|(key, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (key.as_str().to_owned(), value.to_owned()))
            })
            .collect(),
        body: body.to_vec(),
    };
    state.records.lock().unwrap().push(recorded.clone());
    let reply = (state.responder)(&recorded);
    let mut response = Response::builder().status(StatusCode::from_u16(reply.status).unwrap());
    for (key, value) in reply.headers {
        response = response.header(key, value);
    }
    response.body(Body::from(reply.body)).unwrap()
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-cli-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_token(dir: &Path, name: &str, value: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, value).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn job_id() -> String {
    JobId::new().to_string()
}

fn job_json(id: &str, state: &str) -> Value {
    json!({
        "id": id,
        "state": state,
        "spec": {"repo": "/srv/repo", "git_ref": "main", "task": "go"},
        "attempt": 1,
        "created_at": 10,
        "started_at": 11,
        "failure_class": null,
        "summary": null
    })
}

fn run(args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>, cwd: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lu"));
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("LU_URL")
        .env_remove("LU_TOKEN_FILE")
        .env_remove("LU_CA_FILE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let child = match stdin {
        Some(text) => {
            command.stdin(Stdio::piped());
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
            child
        }
        None => {
            command.stdin(Stdio::null());
            command.spawn().unwrap()
        }
    };
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn env_for(server: &TestServer, token: &Path) -> Vec<(String, String)> {
    vec![
        ("LU_URL".to_owned(), server.url.clone()),
        ("LU_TOKEN_FILE".to_owned(), token.display().to_string()),
    ]
}

fn run_with_env(
    args: &[&str],
    envs: &[(String, String)],
    stdin: Option<&str>,
    cwd: &Path,
) -> Output {
    let borrowed: Vec<(&str, &str)> = envs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    run(args, &borrowed, stdin, cwd)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_json_prints_the_raw_body_and_sends_the_filters() {
    let id = job_id();
    let body = json!({"jobs": [job_json(&id, "running")], "future": true});
    let expected = body.to_string();
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("ls-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["--json", "ls", "--state", "running", "--limit", "2"],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[0].path, "/v1/jobs");
    assert_eq!(records[0].query.as_deref(), Some("state=running&limit=2"));
    let authorization = format!("Bearer {TOKEN}");
    assert_eq!(
        records[0].header("authorization"),
        Some(authorization.as_str())
    );
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "tui")]
async fn top_once_prints_tables_and_top_json_prints_one_object() {
    let id = job_id();
    let node_body = json!([{
        "name": "node-a",
        "harness": "idle",
        "providers": [],
        "last_seen": 20,
        "draining": false
    }]);
    let jobs_body = json!({"jobs": [job_json(&id, "running")]});
    let server = start(move |record| {
        if record.path.contains("nodes") {
            Reply::json(&node_body)
        } else {
            Reply::json(&jobs_body)
        }
    })
    .await;
    let dir = scratch("top-once-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["top", "--once"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("NODE"), "{text}");
    assert!(text.contains("node-a"), "{text}");
    assert!(text.contains(&id[id.len() - 10..]), "{text}");
    assert!(!text.contains("lu top"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");

    let output = run_with_env(&["top", "--json"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(value["nodes"][0]["name"], "node-a");
    assert_eq!(value["jobs"][0]["id"], id);

    let output = run_with_env(&["top", "--once", "--json"], &envs, None, &dir);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("mutually exclusive"),
        "{}",
        stderr(&output)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_uses_the_agent_table_and_sanitises_hostile_text() {
    let id = job_id();
    let mut job = job_json(&id, "failed");
    job["node"] = json!("node-a");
    job["failure_class"] = json!("execution");
    job["summary"] = json!("bad tests\n\u{1b}[31m");
    let server = start(move |_| Reply::json(&job)).await;
    let dir = scratch("show-config");
    let token = write_token(&dir, "operator.token", TOKEN);
    let config = dir.join("lu.toml");
    std::fs::write(
        &config,
        format!(
            r#"[agent]
name = "operator"
controller_url = "{}"
token_file = "{}"
work_dir = "/srv/work/lu"
harness = "opencode"
"#,
            server.url,
            token.display()
        ),
    )
    .unwrap();

    let output = run(
        &["--config", config.to_str().unwrap(), "show", &id],
        &[],
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("bad tests??[31m"), "{text}");
    assert!(text.contains("node-a"), "{text}");
    assert!(text.contains("execution"), "{text}");
    let state = text
        .lines()
        .find(|line| line.trim_start().starts_with("state"))
        .unwrap();
    assert!(state.ends_with("failed"), "{state:?}");
    assert_eq!(
        server.records.lock().unwrap()[0].path,
        format!("/v1/jobs/{id}")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_passes_after_dependencies() {
    let id = job_id();
    let dep1 = job_id();
    let dep2 = job_id();
    let body = job_json(&id, "blocked");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-after");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--after",
            &dep1,
            "--after",
            &dep2,
        ],
        &env_for(&server, &token),
        Some("run after the others\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[0].body).unwrap();
    assert_eq!(spec["after"], json!([dep1, dep2]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_no_stack_sets_and_default_omits_the_field() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-no-stack");
    let token = write_token(&dir, "operator.token", TOKEN);

    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--no-stack",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[0].body).unwrap();
    assert_eq!(spec["stack_dependencies"], json!(false));

    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[1].body).unwrap();
    assert!(
        spec.get("stack_dependencies").is_none(),
        "the default must be omitted: {spec}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_shows_the_waiting_dependency_count() {
    let waiting_id = job_id();
    let free_id = job_id();
    let dep = job_id();
    let mut waiting = job_json(&waiting_id, "blocked");
    waiting["after"] = json!([dep]);
    waiting["waiting_on"] = json!([dep]);
    let free = job_json(&free_id, "queued");
    let body = json!({"jobs": [waiting, free]});
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("ls-after");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["ls"], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let header = text.lines().next().unwrap();
    let columns: Vec<&str> = header.split_whitespace().collect();
    assert_eq!(columns[4], "AFTER", "{text}");
    let waiting_line = text
        .lines()
        .find(|line| line.contains(&waiting_id))
        .unwrap();
    let cells: Vec<&str> = waiting_line.split_whitespace().collect();
    assert_eq!(cells[1], "blocked");
    assert_eq!(cells[4], "1");
    let free_line = text.lines().find(|line| line.contains(&free_id)).unwrap();
    let cells: Vec<&str> = free_line.split_whitespace().collect();
    assert_eq!(cells[4], "-");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_lists_dependency_states() {
    let id = job_id();
    let dep = job_id();
    let main_id = id.clone();
    let dep_id = dep.clone();
    let server = start(move |recorded| {
        if recorded.path == format!("/v1/jobs/{dep_id}") {
            Reply::json(&job_json(&dep_id, "blocked"))
        } else {
            let mut view = job_json(&main_id, "blocked");
            view["after"] = json!([dep_id]);
            view["waiting_on"] = json!([dep_id]);
            Reply::json(&view)
        }
    })
    .await;
    let dir = scratch("show-after");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["show", &id], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("after  {dep}  blocked (waiting)")),
        "{text}"
    );
    assert!(
        server
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|record| record.path == format!("/v1/jobs/{dep}")),
        "the dependency should be fetched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_draws_a_diamond_and_prints_its_nodes_as_json() {
    let a = job_id();
    let b = job_id();
    let c = job_id();
    let d = job_id();
    let mut joined = job_json(&d, "blocked");
    joined["after"] = json!([b, c]);
    let mut right = job_json(&c, "failed");
    right["after"] = json!([a]);
    let mut left = job_json(&b, "running");
    left["after"] = json!([a]);
    let root = job_json(&a, "done");
    let body = json!({"jobs": [joined, right, left, root]});
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("graph-diamond");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["graph"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains('✓'), "{text}");
    assert!(text.contains('▶'), "{text}");
    assert!(text.contains('✗'), "{text}");
    assert!(text.contains('◌'), "{text}");
    assert!(text.contains("┐") && text.contains("└"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");

    let output = run_with_env(&["--json", "graph"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    let nodes = value["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 4);
    assert!(nodes
        .iter()
        .any(|node| node["id"] == a && node["state"] == "done"));
    assert!(nodes
        .iter()
        .any(|node| node["id"] == d && node["state"] == "blocked"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_reads_stdin_and_sends_every_flag() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-stdin");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--harness",
            "opencode",
            "--provider",
            "peer-a",
            "--model",
            "big-pickle",
            "--timeout",
            "90",
            "--key",
            "ticket:42",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), id);
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].path, "/v1/jobs");
    assert_eq!(records[0].header("idempotency-key"), Some("ticket:42"));
    let spec: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(spec["repo"], "https://example.net/repo.git");
    assert_eq!(spec["git_ref"], "main");
    assert_eq!(spec["task"], "run the tests\n");
    assert_eq!(spec["provider"], "peer-a");
    assert_eq!(spec["model"], "big-pickle");
    assert_eq!(spec["timeout_secs"], 90);
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_sends_idle_timeout_and_rejects_zero() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-idle");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--idle-timeout",
            "45",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    {
        let records = server.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        let spec: Value = serde_json::from_slice(&records[0].body).unwrap();
        assert_eq!(spec["idle_timeout_secs"], 45);
    }

    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--idle-timeout",
            "0",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(
        server.records.lock().unwrap().len(),
        1,
        "a usage error must not reach the controller"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_reads_a_task_file() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-file");
    let token = write_token(&dir, "operator.token", TOKEN);
    let task = dir.join("task.txt");
    std::fs::write(&task, "from the file").unwrap();
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "/srv/repo",
            "--ref",
            "main",
            "--task-file",
            task.to_str().unwrap(),
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let records = server.records.lock().unwrap();
    let spec: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(spec["task"], "from the file");
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_wait_exits_three_when_the_job_fails() {
    let id = job_id();
    let post_id = id.clone();
    let get_id = id.clone();
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&job_json(&post_id, "queued"))
        } else {
            Reply::json(&job_json(&get_id, "failed"))
        }
    })
    .await;
    let dir = scratch("submit-wait");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--task",
            "go",
            "--wait",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), id);
    assert!(
        stderr(&output).contains("state: failed"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_follow_sanitises_escapes_and_exits_three_on_failure() {
    let id = job_id();
    let log_id = id.clone();
    let calls = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&calls);
    let server = start(move |record| {
        assert!(record.path.ends_with("/logs"));
        let mut count = counter.lock().unwrap();
        *count += 1;
        if *count == 1 {
            assert_eq!(record.query.as_deref(), Some("offset=0&wait=25"));
            Reply::json(&json!({"data": "ok\u{1b}[31m\n", "next": 5, "state": "running"}))
        } else {
            assert_eq!(record.query.as_deref(), Some("offset=5&wait=25"));
            Reply::json(&json!({
                "data": format!("bad {}", log_id),
                "next": 9,
                "state": "failed",
                "terminal": true
            }))
        }
    })
    .await;
    let dir = scratch("logs-follow");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["logs", &id, "--follow"],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let text = stdout(&output);
    assert_eq!(text, format!("ok?[31m\nbad {id}"));
    assert!(!text.contains('\u{1b}'));
    assert_eq!(*calls.lock().unwrap(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_writes_the_fixed_private_names_and_needs_force_to_replace() {
    let id = job_id();
    let server = start(|record| {
        if record.path.ends_with(ArtifactName::DiffPatch.file_name()) {
            Reply::ok(b"diff --git".to_vec())
        } else if record.path.ends_with(ArtifactName::RunLog.file_name()) {
            Reply::ok(b"log line\n".to_vec())
        } else {
            Reply::ok(br#"{"ok":true}"#.to_vec())
        }
    })
    .await;
    let dir = scratch("fetch");
    let token = write_token(&dir, "operator.token", TOKEN);
    let out = dir.join("artifacts");
    let envs = env_for(&server, &token);

    let output = run_with_env(
        &["fetch", &id, "--out", out.to_str().unwrap()],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    for name in ArtifactName::ALL {
        let path = out.join(name.file_name());
        assert!(path.is_file(), "missing {}", path.display());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{} has mode {mode:o}", path.display());
    }
    assert_eq!(
        std::fs::read(out.join("diff.patch")).unwrap(),
        b"diff --git"
    );

    let output = run_with_env(
        &["fetch", &id, "--out", out.to_str().unwrap()],
        &envs,
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );

    let output = run_with_env(
        &["fetch", &id, "--out", out.to_str().unwrap(), "--force"],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let records = server.records.lock().unwrap();
    assert!(records.iter().all(|record| record
        .path
        .starts_with(&format!("/v1/jobs/{id}/artifacts/"))));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_and_drain_render_aligned_text() {
    let list = json!([
        {"name": "node-a", "version": "0.1.10", "harness": "idle", "last_seen": 10, "draining": true},
        {"name": "node-b", "harness": "busy", "last_seen": 12, "draining": false}
    ]);
    let server = start(move |record| {
        if record.path == "/v1/nodes" {
            Reply::json(&list)
        } else if record.path.ends_with("/drain") {
            Reply::status(204)
        } else {
            Reply::json(&json!({"name": "node-a", "draining": false}))
        }
    })
    .await;
    let dir = scratch("nodes");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["nodes"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let header = text.lines().next().unwrap();
    assert!(header.starts_with("NAME"), "{text}");
    assert!(header.contains("VERSION"), "{text}");
    assert!(header.contains("HARNESS"), "{text}");
    assert!(text.contains("node-a"), "{text}");
    assert!(text.contains("0.1.10"), "{text}");
    assert!(text.contains("node-b"), "{text}");
    assert!(text.contains("yes"), "{text}");

    let output = run_with_env(&["drain", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let drain_line = stdout(&output)
        .lines()
        .find(|line| line.starts_with("draining"))
        .unwrap()
        .to_owned();
    assert!(drain_line.ends_with("yes"), "{drain_line:?}");

    let output = run_with_env(&["undrain", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let undrain_line = stdout(&output)
        .lines()
        .find(|line| line.starts_with("draining"))
        .unwrap()
        .to_owned();
    assert!(undrain_line.ends_with("no"), "{undrain_line:?}");

    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
    assert_eq!(paths[0], "/v1/nodes");
    assert_eq!(paths[1], "/v1/nodes/node-a/drain");
    assert_eq!(paths[2], "/v1/nodes/node-a/undrain");
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_prints_a_summary_and_json_prints_the_raw_body() {
    let body = json!({
        "version": "0.1.10",
        "uptime_secs": 42,
        "schema_version": 2,
        "db_bytes": 4096,
        "recovered_on_boot": 1,
        "jobs": {"queued": 2, "running": 1},
        "oldest_queued_secs": 7,
        "nodes": 3,
        "nodes_online": 2,
        "lease_secs": 60,
        "future": true
    });
    let expected = body.to_string();
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("status");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["status"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for (key, value) in [
        ("version", "0.1.10"),
        ("uptime_secs", "42"),
        ("schema_version", "2"),
        ("db_bytes", "4096"),
        ("recovered_on_boot", "1"),
        ("oldest_queued_secs", "7"),
        ("nodes", "3"),
        ("nodes_online", "2"),
        ("lease_secs", "60"),
    ] {
        assert!(
            text.lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>() == [key, value]),
            "{key}: {text}"
        );
    }
    let queued = text
        .lines()
        .find(|line| line.trim_start().starts_with("queued"))
        .unwrap();
    assert!(queued.trim_end().ends_with('2'), "{queued:?}");
    assert!(text.contains("STATE"), "{text}");

    let output = run_with_env(&["--json", "status"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
    assert_eq!(paths, vec!["/v1/status", "/v1/status"]);
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prints_host_numbers_and_indented_process_tree() {
    let id = job_id();
    let stats = json!({
        "host": {
            "load1": 0.5,
            "load5": 0.25,
            "load15": 0.125,
            "mem_total_kb": 8192,
            "mem_available_kb": 4096,
            "swap_used_kb": 12,
            "uptime_secs": 600
        },
        "jobs": [{
            "job_id": id,
            "pgid": 4242,
            "output_idle_secs": 400,
            "cpu_idle_secs": 350,
            "stuck": true,
            "processes": [{
                "pid": 4242,
                "ppid": 1,
                "state": "S",
                "cpu_secs": 7,
                "rss_kb": 1024,
                "age_secs": 90,
                "cmdline": "opencode run\n\u{1b}[31m",
                "open_files": 3
            }]
        }]
    });
    let expected = stats.to_string();
    let server = start(move |_| Reply::json(&stats)).await;
    let dir = scratch("node");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["node", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("node  node-a"), "{text}");
    assert!(matches!(
        text.lines()
            .find(|line| line.starts_with("load1"))
            .and_then(|line| line.split_whitespace().nth(1)),
        Some("0.50")
    ));
    assert!(text.contains(&format!("job {id}")), "{text}");
    assert!(text.contains("stuck yes"), "{text}");
    let process_line = text
        .lines()
        .find(|line| line.trim_start().starts_with("4242"))
        .unwrap();
    assert!(process_line.starts_with("  4242"), "{process_line:?}");
    assert!(
        process_line.contains("PID") || text.contains("  PID"),
        "{text}"
    );
    assert!(!text.contains('\u{1b}'), "{text:?}");

    let output = run_with_env(&["--json", "node", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["/v1/nodes/node-a/stats", "/v1/nodes/node-a/stats"]
    );
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_never_prints_the_token() {
    let id = job_id();
    let echo = format!("the token was {TOKEN}");
    let server = start(move |_| {
        let mut reply = Reply::json(&json!({"error": "unauthorized", "detail": echo}));
        reply.status = 401;
        reply
    })
    .await;
    let dir = scratch("unauthorized");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["show", &id], &env_for(&server, &token), None, &dir);
    assert_eq!(output.status.code(), Some(1));
    assert!(!stdout(&output).contains(TOKEN));
    assert!(!stderr(&output).contains(TOKEN), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unauthorized"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_exit_two_before_any_request() {
    let dir = scratch("usage");
    let output = run(&["show", "not-an-id"], &[], None, &dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("job id"), "{}", stderr(&output));

    let output = run(&["ls", "--state", "paused"], &[], None, &dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("unknown job state"),
        "{}",
        stderr(&output)
    );

    let output = run(
        &[
            "submit",
            "--repo",
            "/r",
            "--ref",
            "main",
            "--task",
            "a",
            "--task-file",
            "b",
        ],
        &[],
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

fn finished_job(id: &str, state: &str) -> Value {
    let mut job = job_json(id, state);
    job["spec"] = json!({
        "repo": "/srv/repo",
        "git_ref": "main",
        "task": "go",
        "provider": "peer-a",
        "model": "old-model",
        "timeout_secs": 60,
        "idle_timeout_secs": 45,
        "idempotency_key": "ticket:7",
        "after": [id]
    });
    job
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_resubmits_the_finished_spec_and_applies_the_overrides() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "failed");
    let created = job_json(&new_id, "queued");
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry");
    let token = write_token(&dir, "operator.token", TOKEN);
    let dep = job_id();
    let output = run_with_env(
        &[
            "retry",
            &id,
            "--provider",
            "peer-b",
            "--model",
            "new-model",
            "--after",
            &dep,
            "--key",
            "ticket:42",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), new_id);
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[0].path, format!("/v1/jobs/{id}"));
    assert_eq!(records[1].method, "POST");
    assert_eq!(records[1].path, "/v1/jobs");
    assert_eq!(records[1].header("idempotency-key"), Some("ticket:42"));
    let spec: Value = serde_json::from_slice(&records[1].body).unwrap();
    assert_eq!(spec["repo"], "/srv/repo");
    assert_eq!(spec["git_ref"], "main");
    assert_eq!(spec["task"], "go");
    assert_eq!(spec["harness"], "opencode");
    assert_eq!(spec["timeout_secs"], 60);
    assert_eq!(spec["idle_timeout_secs"], 45);
    assert_eq!(spec["provider"], "peer-b");
    assert_eq!(spec["model"], "new-model");
    assert_eq!(spec["after"], json!([dep]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_keeps_the_old_spec_and_sends_no_idempotency_key_by_default() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "cancelled");
    let created = job_json(&new_id, "queued");
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-plain");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["retry", &id], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), new_id);
    let records = server.records.lock().unwrap();
    assert_eq!(records[1].header("idempotency-key"), None);
    let spec: Value = serde_json::from_slice(&records[1].body).unwrap();
    assert!(spec.get("idempotency_key").is_none(), "{spec}");
    assert_eq!(spec["provider"], "peer-a");
    assert_eq!(spec["model"], "old-model");
    assert_eq!(spec["after"], json!([id]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_refuses_a_job_that_is_not_finished() {
    let id = job_id();
    let body = job_json(&id, "running");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("retry-not-terminal");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["retry", &id], &env_for(&server, &token), None, &dir);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("retry resubmits a finished job"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        server.records.lock().unwrap().len(),
        1,
        "a refusal must not submit anything"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_json_prints_the_raw_submit_body() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "done");
    let created = job_json(&new_id, "queued");
    let expected = created.to_string();
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["--json", "retry", &id],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_wait_reports_the_new_jobs_state_and_exit_code() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "done");
    let post_id = new_id.clone();
    let poll_id = new_id.clone();
    let old_path = id.clone();
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&job_json(&post_id, "queued"))
        } else if record.path == format!("/v1/jobs/{old_path}") {
            Reply::json(&finished)
        } else {
            Reply::json(&job_json(&poll_id, "failed"))
        }
    })
    .await;
    let dir = scratch("retry-wait");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["retry", &id, "--wait"], &envs, None, &dir);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), new_id);
    assert!(
        stderr(&output).contains("state: failed"),
        "{}",
        stderr(&output)
    );

    let output = run_with_env(&["--json", "retry", &id, "--wait"], &envs, None, &dir);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(&new_id), "{text}");
    assert!(text.contains(r#""state":"failed""#), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
