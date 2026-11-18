#![allow(dead_code)]

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
use lu_proto::JobId;
use serde_json::{json, Value};
use tokio::net::TcpListener;

pub const TOKEN: &str = "cli-operator-token-do-not-print";

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn json(value: &Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: value.to_string().into_bytes(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

pub type Responder = dyn Fn(&Recorded) -> Reply + Send + Sync;

#[derive(Clone)]
pub struct FakeState {
    pub records: Arc<Mutex<Vec<Recorded>>>,
    pub responder: Arc<Responder>,
}

pub struct TestServer {
    pub url: String,
    pub records: Arc<Mutex<Vec<Recorded>>>,
    pub handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub async fn start<F>(responder: F) -> TestServer
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

pub async fn fallback(
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

pub fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-cli-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn write_token(dir: &Path, name: &str, value: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, value).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

pub fn job_id() -> String {
    JobId::new().to_string()
}

pub fn job_json(id: &str, state: &str) -> Value {
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

pub fn run(args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>, cwd: &Path) -> Output {
    let exe = std::env::var("CARGO_BIN_EXE_lu")
        .expect("cargo sets the binary path for integration tests");
    let mut command = Command::new(exe);
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

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn env_for(server: &TestServer, token: &Path) -> Vec<(String, String)> {
    vec![
        ("LU_URL".to_owned(), server.url.clone()),
        ("LU_TOKEN_FILE".to_owned(), token.display().to_string()),
    ]
}

pub fn run_with_env(
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
