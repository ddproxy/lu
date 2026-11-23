//! Operator stats API and `/metrics` exposition tests.

use axum::Router;
use lu_auth::sha256_hex;
use lu_config::{ControllerAgent, ControllerConfig, ControllerOperator};
use lu_controller::{build_router, AppState};
use lu_proto::{CollectMode, JobId, JobSpec, SourceSpec, DEFAULT_PRIORITY};
use lu_serve::{serve, ServeOptions};
use lu_store::sqlite::SqliteStore;
use rusqlite::params;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

struct TestServer {
    addr: SocketAddr,
    token: String,
    store: Arc<SqliteStore>,
    db: PathBuf,
    handle: JoinHandle<()>,
    _tmp: TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn runtime_token(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{label}-{}-{nanos}", std::process::id())
}

fn config(db: &Path, artifacts: &Path, token: &str) -> ControllerConfig {
    let agent_tokens: Vec<String> = ["node-a", "node-b", "node-c"]
        .iter()
        .map(|node| runtime_token(node))
        .collect();
    ControllerConfig {
        listen: "127.0.0.1:0".to_owned(),
        db: db.to_string_lossy().into_owned(),
        artifacts_dir: artifacts.to_string_lossy().into_owned(),
        lease_secs: 6,
        max_retries: 1,
        provider_cooldown_secs: 60,
        recovery_grace_secs: 1,
        max_connections: 64,
        max_connections_per_addr: 32,
        local_repo_roots: Vec::new(),
        agents: ["node-a", "node-b", "node-c"]
            .iter()
            .zip(agent_tokens)
            .map(|(name, agent_token)| ControllerAgent {
                name: (*name).to_owned(),
                token_sha256: sha256_hex(&agent_token),
            })
            .collect(),
        operators: vec![ControllerOperator {
            name: "ops".to_owned(),
            token_sha256: sha256_hex(token),
            ..Default::default()
        }],
        repos: Vec::new(),
        groups: Vec::new(),
        blocked_dependent_ttl: "24h".to_owned(),
    }
}

async fn start() -> TestServer {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("stats.db");
    let artifacts = tmp.path().join("artifacts");
    let token = runtime_token("operator");
    let cfg = config(&db, &artifacts, &token);
    let store = Arc::new(SqliteStore::open(&db).unwrap());
    let store_dyn: Arc<dyn lu_store::Store> = store.clone();
    let state = Arc::new(AppState::from_config(&cfg, store_dyn).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router: Router = build_router(state);
    let handle = tokio::spawn(async move {
        let options = ServeOptions {
            max_connections: 16,
            max_connections_per_addr: 8,
            header_read_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            connection_idle_timeout: Duration::from_secs(30),
        };
        let _ = serve(
            listener,
            router,
            options,
            None,
            std::future::pending::<()>(),
        )
        .await;
    });
    TestServer {
        addr,
        token,
        store,
        db,
        handle,
        _tmp: tmp,
    }
}

fn spec(repo: &str, key: &str) -> JobSpec {
    JobSpec {
        source: SourceSpec {
            repo: repo.to_owned(),
            git_ref: "main".to_owned(),
            base: None,
        },
        repo: repo.to_owned(),
        git_ref: "main".to_owned(),
        base_commit: None,
        task: "work".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 3600,
        idle_timeout_secs: None,
        on_stuck: None,
        stuck_after_secs: None,
        idempotency_key: Some(key.to_owned()),
        after: Vec::new(),
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
        collect: CollectMode::Apply,
        include_failed: false,
        on_conflict: Default::default(),
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        fallbacks: Vec::new(),
        expect: None,
        priority: DEFAULT_PRIORITY,
        group: None,
        allow_spawn: false,
        spawn_inherit: false,
        spawn_repos: Vec::new(),
        workspace: None,
        cache_dirs: Vec::new(),
        context: None,
        origin: None,
        origin_ref: None,
        untrusted_context: false,
        draft: false,
        publish: None,
    }
}

struct TerminalJob<'a> {
    key: &'a str,
    repo: &'a str,
    node: &'a str,
    state: &'a str,
    failure: Option<&'a str>,
    attempt: u32,
    created: i64,
    started: Option<i64>,
    finished: Option<i64>,
}

fn insert_terminal(server: &TestServer, job: TerminalJob<'_>) -> JobId {
    use lu_store::{Now, Store};

    let TerminalJob {
        key,
        repo,
        node,
        state,
        failure,
        attempt,
        created,
        started,
        finished,
    } = job;
    let (job, _) = server
        .store
        .create_job(
            &spec(repo, key),
            Now {
                wall_unix: created,
                mono_ms: created.max(0) as u64,
            },
        )
        .unwrap();
    let conn = rusqlite::Connection::open(&server.db).unwrap();
    conn.execute(
        "UPDATE jobs SET state=?2,node=?3,attempt=?4,started_at=?5,finished_at=?6,
                         failure_class=?7 WHERE id=?1",
        params![
            job.id.as_str(),
            state,
            node,
            attempt as i64,
            started,
            finished,
            failure,
        ],
    )
    .unwrap();
    job.id
}

async fn request(addr: SocketAddr, path: &str, token: Option<&str>) -> String {
    let auth = token
        .map(|value| format!("Authorization: Bearer {value}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{auth}\r\n");
    let mut stream = None;
    for _ in 0..50 {
        match TcpStream::connect(addr).await {
            Ok(socket) => {
                stream = Some(socket);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    let mut stream = stream.expect("test server did not start");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn status(resp: &str) -> u16 {
    resp.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn body(resp: &str) -> &str {
    resp.split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_api_authenticates_and_validates_parameters() {
    let server = start().await;
    insert_terminal(
        &server,
        TerminalJob {
            key: "api-done",
            repo: "https://git.example.test/team/app.git",
            node: "node-a",
            state: "done",
            failure: None,
            attempt: 1,
            created: 100,
            started: Some(110),
            finished: Some(160),
        },
    );

    let anonymous = request(server.addr, "/v1/stats", None).await;
    assert_eq!(status(&anonymous), 401);
    let wrong = runtime_token("wrong");
    let denied = request(server.addr, "/v1/stats", Some(&wrong)).await;
    assert_eq!(status(&denied), 401);

    for path in [
        "/v1/stats?by=nope",
        "/v1/stats?since=not-a-time",
        "/v1/stats?node=bad%21name",
    ] {
        let resp = request(server.addr, path, Some(&server.token)).await;
        assert_eq!(status(&resp), 400, "{path}: {resp}");
    }
    let ok = request(
        server.addr,
        "/v1/stats?by=repo&since=0&node=node-a",
        Some(&server.token),
    )
    .await;
    assert_eq!(status(&ok), 200, "{ok}");
    let report: Value = serde_json::from_str(body(&ok)).unwrap();
    assert_eq!(report["by"], "repo");
    assert_eq!(report["node"], "node-a");
    assert_eq!(report["since"], 0);
    assert_eq!(report["fleet"]["states"]["done"], 1);
    let nodes = report["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["node"], "node-a");
    assert_eq!(nodes[0]["busy_seconds"], 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_expose_fixed_node_labels_without_job_content() {
    let server = start().await;
    let marker = "unique job content marker";
    insert_terminal(
        &server,
        TerminalJob {
            key: "metric-done",
            repo: &format!(
                "https://git.example.test/team/{}.git",
                marker.replace(' ', "-")
            ),
            node: "node-a",
            state: "done",
            failure: None,
            attempt: 2,
            created: 100,
            started: Some(110),
            finished: Some(170),
        },
    );
    insert_terminal(
        &server,
        TerminalJob {
            key: "metric-fail",
            repo: "https://git.example.test/team/app.git",
            node: "node-a",
            state: "failed",
            failure: Some("provider"),
            attempt: 1,
            created: 200,
            started: Some(210),
            finished: Some(270),
        },
    );
    insert_terminal(
        &server,
        TerminalJob {
            key: "metric-other",
            repo: "https://git.example.test/team/other.git",
            node: "node-b",
            state: "done",
            failure: None,
            attempt: 1,
            created: 300,
            started: Some(320),
            finished: Some(380),
        },
    );

    let metric = request(server.addr, "/metrics", Some(&server.token)).await;
    assert_eq!(status(&metric), 200);
    let text = body(&metric);
    for expected in [
        "lu_node_jobs_total{node=\"node-a\",state=\"done\"} 1",
        "lu_node_jobs_total{node=\"node-a\",state=\"failed\"} 1",
        "lu_node_failures_total{node=\"node-a\",class=\"provider\"} 1",
        "lu_node_busy_seconds_total{node=\"node-a\"} 120",
        "lu_node_job_runtime_seconds_bucket{node=\"node-a\",le=\"60\"} 2",
        "lu_node_queue_wait_seconds_bucket{node=\"node-b\",le=\"60\"} 1",
        "lu_node_last_job_finished_timestamp_seconds{node=\"node-c\"} 0",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in metrics");
    }
    assert!(!text.contains(marker));
    assert!(!text.contains("git.example.test/team"));
    for line in text.lines() {
        if let Some(start) = line.find("node=\"") {
            let rest = &line[start + 6..];
            let end = rest.find('"').unwrap();
            let node = &rest[..end];
            assert!(
                matches!(node, "node-a" | "node-b" | "node-c"),
                "unconfigured node label {node:?}"
            );
        }
        if let Some(start) = line.find("class=\"") {
            let rest = &line[start + 7..];
            let end = rest.find('"').unwrap();
            let class = &rest[..end];
            assert!(
                matches!(
                    class,
                    "node" | "provider" | "execution" | "dependency" | "conflict"
                ),
                "unexpected failure class label {class:?}"
            );
        }
    }

    // A later job only raises the persistent-source counters.
    insert_terminal(
        &server,
        TerminalJob {
            key: "metric-later",
            repo: "https://git.example.test/team/app.git",
            node: "node-b",
            state: "failed",
            failure: Some("execution"),
            attempt: 1,
            created: 400,
            started: Some(410),
            finished: Some(470),
        },
    );
    let again = request(server.addr, "/metrics", Some(&server.token)).await;
    let later = body(&again);
    assert!(later.contains("lu_node_jobs_total{node=\"node-b\",state=\"failed\"} 1"));
    assert!(later.contains("lu_node_busy_seconds_total{node=\"node-a\"} 120"));
}
