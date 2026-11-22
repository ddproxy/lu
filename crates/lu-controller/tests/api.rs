use git2::Signature;
use lu_auth::sha256_hex;
use lu_config::{
    ArtifactsConfig, ControllerAgent, ControllerConfig, ControllerOperator, ControllerRepo,
    ScrubRuleConfig,
};
use lu_controller::limits::{PACK_PER_TOKEN, SYNC_PER_TOKEN};
use lu_controller::{build_router, recover_on_boot, run_background, AppState};
use lu_proto::{
    Choice, JobEdit, JobId, JobSpec, NodeName, StuckPolicy, MAX_AFTER_JOBS, MAX_FALLBACKS,
    MAX_LOG_BYTES, MAX_REPORT_BYTES, MAX_REQUIRES_LABELS, MAX_TOLERATES,
};
use lu_serve::{serve, ServeOptions};
use lu_store::sqlite::SqliteStore;
use lu_store::{CordonSource, Store};
use serde_json::json;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

mod common;

const OP_TOKEN: &str = "op-token-1234567890";
const AGENT_A_TOKEN: &str = "agent-a-token-1234567890";
const AGENT_B_TOKEN: &str = "agent-b-token-1234567890";

fn controller_cfg(db: &str, artifacts_dir: &str) -> ControllerConfig {
    ControllerConfig {
        listen: "127.0.0.1:0".to_owned(),
        db: db.to_owned(),
        artifacts_dir: artifacts_dir.to_owned(),
        lease_secs: 6,
        max_retries: 1,
        provider_cooldown_secs: 60,
        recovery_grace_secs: 1,
        max_connections: 64,
        max_connections_per_addr: 32,
        local_repo_roots: Vec::new(),
        agents: vec![
            ControllerAgent {
                name: "node-a".to_owned(),
                token_sha256: sha256_hex(AGENT_A_TOKEN),
            },
            ControllerAgent {
                name: "node-b".to_owned(),
                token_sha256: sha256_hex(AGENT_B_TOKEN),
            },
        ],
        operators: vec![ControllerOperator {
            name: "ops".to_owned(),
            token_sha256: sha256_hex(OP_TOKEN),
        }],
        repos: Vec::new(),
        groups: Vec::new(),
        blocked_dependent_ttl: "24h".to_owned(),
    }
}

struct TestServer {
    addr: SocketAddr,
    shutdown: broadcast::Sender<()>,
    state: Arc<AppState>,
    store: Arc<SqliteStore>,
    _handle: tokio::task::JoinHandle<()>,
    _bg: tokio::task::JoinHandle<()>,
    _artifacts_dir: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

async fn start_server() -> TestServer {
    start_server_with(Vec::new(), None).await
}

async fn start_server_with(roots: Vec<String>, pack_cap: Option<usize>) -> TestServer {
    start_server_full(roots, pack_cap, Vec::new()).await
}

async fn start_server_with_repos(repos: Vec<ControllerRepo>) -> TestServer {
    start_server_full(Vec::new(), None, repos).await
}

async fn start_server_full(
    roots: Vec<String>,
    pack_cap: Option<usize>,
    repos: Vec<ControllerRepo>,
) -> TestServer {
    start_server_inner(roots, pack_cap, repos, None).await
}

async fn start_server_with_artifacts(artifacts: Option<ArtifactsConfig>) -> TestServer {
    start_server_inner(Vec::new(), None, Vec::new(), artifacts).await
}

async fn start_server_inner(
    roots: Vec<String>,
    pack_cap: Option<usize>,
    repos: Vec<ControllerRepo>,
    artifacts: Option<ArtifactsConfig>,
) -> TestServer {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("lu.db");
    let artifacts_dir = tmp.path().join("artifacts");
    let mut cfg = controller_cfg(db.to_str().unwrap(), artifacts_dir.to_str().unwrap());
    cfg.local_repo_roots = roots;
    cfg.repos = repos;
    let store = Arc::new(SqliteStore::open(&db).unwrap());
    let mut state =
        AppState::from_config_with_artifacts(&cfg, store.clone(), artifacts.as_ref()).unwrap();
    state.max_pack_bytes = pack_cap.unwrap_or(lu_staging::PACK_CAP_BYTES);
    let state = Arc::new(state);
    recover_on_boot(&state).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, _) = broadcast::channel::<()>(4);
    let mut rx1 = tx.subscribe();
    let mut rx2 = tx.subscribe();
    let options = ServeOptions {
        max_connections: 32,
        max_connections_per_addr: 16,
        header_read_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        connection_idle_timeout: Duration::from_secs(30),
    };
    let bg_state = Arc::clone(&state);
    let bg = tokio::spawn(async move {
        run_background(bg_state, async move {
            let _ = rx2.recv().await;
        })
        .await;
    });
    let handle = tokio::spawn(serve(
        listener,
        build_router(Arc::clone(&state)),
        options,
        None,
        async move {
            let _ = rx1.recv().await;
        },
    ));
    TestServer {
        addr,
        shutdown: tx,
        state,
        store,
        _handle: handle,
        _bg: bg,
        _artifacts_dir: artifacts_dir,
        _tmp: tmp,
    }
}

async fn raw_request(addr: SocketAddr, req: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn raw_request_bytes(addr: SocketAddr, req: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    out
}

fn status(resp: &str) -> u16 {
    resp.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}
fn header_value<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    for line in resp.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().to_ascii_lowercase() == lower {
                return Some(v.trim());
            }
        }
        if line.is_empty() {
            break;
        }
    }
    None
}
fn body(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}
fn body_str(resp: &str) -> &str {
    body(resp)
}

fn sample_spec(key: Option<&str>) -> JobSpec {
    JobSpec {
        source: lu_proto::SourceSpec {
            repo: "https://example.net/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            base: None,
        },
        repo: "https://example.net/repo.git".to_owned(),
        git_ref: "main".to_owned(),
        base_commit: None,
        task: "run tests".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 3600,
        idle_timeout_secs: None,
        idempotency_key: key.map(|s| s.to_owned()),
        after: Vec::new(),
        collect: lu_proto::CollectMode::Apply,
        include_failed: false,
        on_conflict: Default::default(),
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        on_stuck: None,
        stuck_after_secs: None,
        priority: lu_proto::DEFAULT_PRIORITY,
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
        fallbacks: Vec::new(),
        expect: None,
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
    }
}

async fn submit_spec(addr: SocketAddr, spec: &JobSpec) -> String {
    let body = serde_json::to_string(spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    raw_request(addr, &req).await
}

async fn edit_job(addr: SocketAddr, id: &str, edit: &JobEdit) -> String {
    let body = serde_json::to_string(edit).unwrap();
    let req = format!(
        "POST /v1/jobs/{id}/edit HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    raw_request(addr, &req).await
}

#[tokio::test]
async fn health_is_unauthenticated_and_has_version() {
    let srv = start_server().await;
    let resp = raw_request(
        srv.addr,
        "GET /v1/health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&resp), 200, "{resp}");
    assert!(header_value(&resp, "x-lu-version").is_some(), "{resp}");
    let b = body(&resp);
    let v: serde_json::Value = serde_json::from_str(b).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["db"], "ok");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn auth_matrix_operator_routes() {
    let srv = start_server().await;
    // no token
    let r = raw_request(
        srv.addr,
        "GET /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&r), 401, "{r}");
    // wrong scope (agent token on operator route)
    let r = raw_request(srv.addr, &format!("GET /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 401, "{r}");
    // valid operator
    let r = raw_request(srv.addr, &format!("GET /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn create_list_get_cancel_job_happy_path_and_idempotency() {
    let srv = start_server().await;
    let spec = sample_spec(Some("k1"));
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let j1: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j1["id"].as_str().unwrap().to_string();
    // idempotent same key/spec -> 200 existing
    let r2 = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r2), 200, "{r2}");
    let j2: serde_json::Value = serde_json::from_str(body_str(&r2)).unwrap();
    assert_eq!(j2["id"], j1["id"]);
    // same key different spec -> 409 conflict
    let mut spec2 = spec.clone();
    spec2.task = "different".to_owned();
    let body2 = serde_json::to_string(&spec2).unwrap();
    let req2 = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body2.len(), body2
    );
    let r3 = raw_request(srv.addr, &req2).await;
    assert_eq!(status(&r3), 409, "{r3}");

    // list
    let r = raw_request(srv.addr, &format!("GET /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let list: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert!(list["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|j| j["id"] == id));

    // get
    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");

    // cancel queued -> cancelled
    let r = raw_request(srv.addr, &format!("POST /v1/jobs/{id}/cancel HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let jc: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(jc["state"], "cancelled");

    // terminal cancel -> 409
    let r = raw_request(srv.addr, &format!("POST /v1/jobs/{id}/cancel HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n")).await;
    assert_eq!(status(&r), 409, "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn tolerations_are_validated() {
    let srv = start_server().await;
    for tolerates in [
        (0..=MAX_TOLERATES)
            .map(|index| format!("taint-{index}"))
            .collect::<Vec<_>>(),
        vec!["homelab".to_owned(), "homelab".to_owned()],
        vec!["bad taint".to_owned()],
    ] {
        let mut spec = sample_spec(None);
        spec.tolerates = tolerates.clone();
        let r = submit_spec(srv.addr, &spec).await;
        assert_eq!(status(&r), 422, "{tolerates:?}: {r}");
    }
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn tainted_nodes_are_shown_and_explained() {
    let srv = start_server().await;
    let mut spec = sample_spec(None);
    spec.requires = vec!["gpu".to_owned()];
    let r = submit_spec(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let created: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["waiting_reason"], "no node with label gpu", "{r}");

    let hb = json!({
        "node": "node-a",
        "harness": "idle",
        "labels": ["gpu"],
        "taints": ["homelab"],
    });
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &hb).await;
    assert_eq!(status(&r), 204, "{r}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(
        job["waiting_reason"], "only tainted nodes are free: homelab",
        "{r}"
    );

    let r = raw_request(srv.addr, &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let nodes: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let node = nodes
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "node-a")
        .unwrap();
    assert_eq!(node["taints"], json!(["homelab"]), "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn label_requirements_are_validated_and_explained() {
    let srv = start_server().await;

    for requires in [
        (0..=MAX_REQUIRES_LABELS)
            .map(|index| format!("label-{index}"))
            .collect::<Vec<_>>(),
        vec!["gpu".to_owned(), "gpu".to_owned()],
        vec!["bad label".to_owned()],
    ] {
        let mut spec = sample_spec(None);
        spec.requires = requires.clone();
        let r = submit_spec(srv.addr, &spec).await;
        assert_eq!(status(&r), 422, "{requires:?}: {r}");
    }

    let mut spec = sample_spec(None);
    spec.requires = vec!["php8.3".to_owned(), "gpu".to_owned()];
    let r = submit_spec(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let created: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(
        created["warnings"],
        json!(["no node with label php8.3", "no node with label gpu"]),
        "{r}"
    );
    assert_eq!(
        created["waiting_reason"], "no node with label php8.3",
        "{r}"
    );
    let id = created["id"].as_str().unwrap().to_owned();

    let hb = json!({
        "node": "node-a",
        "harness": "idle",
        "providers": [],
        "labels": ["gpu", "php8.3", "region=eu"],
        "label_sources": {"php8.3": "probed"},
    });
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &hb).await;
    assert_eq!(status(&r), 200, "{r}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["spec"]["requires"], json!(["php8.3", "gpu"]), "{r}");
    assert_eq!(job["waiting_reason"], serde_json::Value::Null, "{r}");

    let r = raw_request(srv.addr, &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let nodes: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let node = nodes
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "node-a")
        .unwrap();
    assert_eq!(node["labels"], json!(["gpu", "php8.3", "region=eu"]), "{r}");
    assert_eq!(node["label_sources"], json!({"php8.3": "probed"}), "{r}");

    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let b = body_str(&r);
    for label in ["gpu", "php8.3", "region=eu"] {
        assert!(
            b.contains(&format!(
                "lu_node_labels_info{{node=\"node-a\",label=\"{label}\"}} 1"
            )),
            "{b}"
        );
    }

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn fallback_choices_are_validated_and_assigned() {
    let srv = start_server().await;

    let too_many = (0..=MAX_FALLBACKS)
        .map(|index| Choice {
            provider: format!("p{index}"),
            model: None,
        })
        .collect::<Vec<_>>();
    let mut cases = Vec::new();
    let mut spec = sample_spec(None);
    spec.provider = Some("primary".to_owned());
    spec.fallbacks = too_many;
    cases.push(spec);
    let mut spec = sample_spec(None);
    spec.provider = Some("p1".to_owned());
    spec.fallbacks = vec![Choice {
        provider: "p1".to_owned(),
        model: None,
    }];
    cases.push(spec);
    let mut spec = sample_spec(None);
    spec.provider = Some("p1".to_owned());
    spec.fallbacks = vec![
        Choice {
            provider: "p2".to_owned(),
            model: None,
        },
        Choice {
            provider: "p2".to_owned(),
            model: Some("m2".to_owned()),
        },
    ];
    cases.push(spec);
    let mut spec = sample_spec(None);
    spec.provider = Some("p1".to_owned());
    spec.fallbacks = vec![Choice {
        provider: "bad name".to_owned(),
        model: None,
    }];
    cases.push(spec);
    for spec in cases {
        let r = submit_spec(srv.addr, &spec).await;
        assert_eq!(status(&r), 422, "{r}");
    }

    let mut spec = sample_spec(None);
    spec.provider = Some("p1".to_owned());
    spec.model = Some("m1".to_owned());
    spec.fallbacks = vec![Choice {
        provider: "p2".to_owned(),
        model: Some("m2".to_owned()),
    }];
    let r = submit_spec(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let created: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = created["id"].as_str().unwrap().to_owned();

    let hb = json!({
        "node": "node-a",
        "harness": "idle",
        "providers": [
            {"name": "p1", "state": "cooling"},
            {"name": "p2", "state": "ok"},
        ],
    });
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &hb).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "assign")
        .unwrap_or_else(|| panic!("no assignment: {events}"));
    assert_eq!(assign["job_id"], id);
    assert_eq!(assign["provider"], "p2", "{assign}");
    assert_eq!(assign["model"], "m2", "{assign}");
    assert_eq!(assign["spec"]["model"], "m1", "{assign}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["assigned_provider"], "p2", "{job}");
    assert_eq!(job["assigned_model"], "m2", "{job}");
    assert_eq!(job["spec"]["fallbacks"][0]["provider"], "p2", "{job}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn post_edit_changes_a_queued_job_and_rejects_bad_states() {
    let srv = start_server().await;
    let r = submit_spec(srv.addr, &sample_spec(None)).await;
    assert_eq!(status(&r), 201, "{r}");
    let created: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = created["id"].as_str().unwrap().to_owned();

    let edit = JobEdit {
        provider: Some("p2".to_owned()),
        model: Some("m2".to_owned()),
        fallbacks: Some(vec![Choice {
            provider: "p3".to_owned(),
            model: Some("m3".to_owned()),
        }]),
        requires: Some(vec!["gpu".to_owned()]),
        on_stuck: None,
        stuck_after_secs: None,
        tolerates: Some(vec!["homelab".to_owned()]),
        after: None,
        priority: None,
        group: None,
        allow_spawn: Some(false),
        spawn_inherit: Some(false),
        spawn_repos: Some(Vec::new()),
        collect: None,
        include_failed: None,
        expect: None,
        on_conflict: Default::default(),
        publish: None,
        source_ref: None,
        task: None,
    };
    let r = edit_job(srv.addr, &id, &edit).await;
    assert_eq!(status(&r), 200, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["id"], id);
    assert_eq!(job["spec"]["provider"], "p2", "{job}");
    assert_eq!(job["spec"]["model"], "m2", "{job}");
    assert_eq!(job["spec"]["fallbacks"][0]["provider"], "p3", "{job}");
    assert_eq!(job["spec"]["requires"][0], "gpu", "{job}");
    assert_eq!(job["spec"]["tolerates"][0], "homelab", "{job}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    let stored: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(stored["spec"], job["spec"], "{stored}");

    // Self-dependency and unknown dependencies are refused with distinct codes.
    let self_edit = JobEdit {
        after: Some(vec![JobId::parse(&id).unwrap()]),
        ..JobEdit::default()
    };
    let r = edit_job(srv.addr, &id, &self_edit).await;
    assert_eq!(status(&r), 400, "{r}");
    let missing = JobEdit {
        after: Some(vec![JobId::new()]),
        ..JobEdit::default()
    };
    let r = edit_job(srv.addr, &id, &missing).await;
    assert_eq!(status(&r), 422, "{r}");

    // An explicit model with `any` is an invalid spec.
    let invalid = JobEdit {
        provider: Some(lu_proto::ANY_PROVIDER.to_owned()),
        model: Some("m".to_owned()),
        ..JobEdit::default()
    };
    let r = edit_job(srv.addr, &id, &invalid).await;
    assert_eq!(status(&r), 422, "{r}");

    // The route needs the operator scope.
    let body = serde_json::to_string(&JobEdit::default()).unwrap();
    let r = raw_request(srv.addr, &format!("POST /v1/jobs/{id}/edit HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)).await;
    assert_eq!(status(&r), 401, "{r}");
    let r = raw_request(srv.addr, &format!("POST /v1/jobs/{id}/edit HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)).await;
    assert_eq!(status(&r), 401, "{r}");

    let r = raw_request(srv.addr, &format!("POST /v1/jobs/{id}/cancel HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let r = edit_job(srv.addr, &id, &JobEdit::default()).await;
    assert_eq!(status(&r), 409, "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn post_edit_updates_dependencies_without_a_cycle() {
    let srv = start_server().await;
    let r = submit_spec(srv.addr, &sample_spec(None)).await;
    let first: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let first_id = first["id"].as_str().unwrap().to_owned();
    let r = submit_spec(srv.addr, &sample_spec(None)).await;
    let second: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let second_id = second["id"].as_str().unwrap().to_owned();

    let edit = JobEdit {
        after: Some(vec![JobId::parse(&first_id).unwrap()]),
        ..JobEdit::default()
    };
    let r = edit_job(srv.addr, &second_id, &edit).await;
    assert_eq!(status(&r), 200, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["state"], "blocked", "{job}");
    assert_eq!(job["after"][0], first_id, "{job}");

    let edit = JobEdit {
        after: Some(vec![JobId::parse(&second_id).unwrap()]),
        ..JobEdit::default()
    };
    let r = edit_job(srv.addr, &first_id, &edit).await;
    assert_eq!(status(&r), 400, "{r}");

    let r = edit_job(
        srv.addr,
        &second_id,
        &JobEdit {
            after: Some(Vec::new()),
            ..JobEdit::default()
        },
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["state"], "queued", "{job}");
    assert!(job["after"].as_array().unwrap().is_empty(), "{job}");

    // The first job still exists with its original id, so dependents stay attached.
    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{first_id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn any_provider_dispatches_a_published_healthy_pair() {
    let srv = start_server().await;
    let mut spec = sample_spec(None);
    spec.provider = Some(lu_proto::ANY_PROVIDER.to_owned());
    let r = submit_spec(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let created: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = created["id"].as_str().unwrap().to_owned();

    let mut invalid = spec.clone();
    invalid.model = Some("m1".to_owned());
    assert_eq!(status(&submit_spec(srv.addr, &invalid).await), 422);
    let mut invalid = spec.clone();
    invalid.fallbacks = vec![Choice {
        provider: "p2".to_owned(),
        model: None,
    }];
    assert_eq!(status(&submit_spec(srv.addr, &invalid).await), 422);

    let hb = json!({
        "node": "node-a",
        "harness": "idle",
        "providers": [
            {"name": "p1", "state": "cooling"},
            {"name": "p2", "state": "ok"},
        ],
        "provider_models": [
            {"provider": "p1", "model": "m1"},
            {"provider": "p2", "model": "m2"},
        ],
    });
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &hb).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "assign")
        .unwrap_or_else(|| panic!("no assignment: {events}"));
    assert_eq!(assign["job_id"], id, "{assign}");
    assert_eq!(assign["provider"], "p2", "{assign}");
    assert_eq!(assign["model"], "m2", "{assign}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["assigned_provider"], "p2", "{job}");
    assert_eq!(job["assigned_model"], "m2", "{job}");

    let r = raw_request(srv.addr, &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    let nodes: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let node = nodes
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "node-a")
        .unwrap();
    assert_eq!(node["provider_models"][0]["provider"], "p1", "{node}");
    assert_eq!(node["provider_models"][1]["model"], "m2", "{node}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn any_provider_without_a_healthy_published_pair_waits() {
    let srv = start_server().await;
    let mut spec = sample_spec(None);
    spec.provider = Some(lu_proto::ANY_PROVIDER.to_owned());
    let r = submit_spec(srv.addr, &spec).await;
    let created: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = created["id"].as_str().unwrap().to_owned();

    let hb = json!({
        "node": "node-a",
        "harness": "idle",
        "providers": [{"name": "p1", "state": "ok"}],
    });
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &hb).await;
    assert_eq!(status(&r), 204, "{r}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["state"], "queued", "{job}");
    assert!(
        job["waiting_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("any")),
        "{job}"
    );

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn list_jobs_by_key_returns_zero_or_one_job() {
    let srv = start_server().await;
    let spec = sample_spec(Some("build"));
    let payload = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        payload.len(), payload
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let created: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();

    for (query, expected) in [("key=build", 200), ("key=missing", 200), ("key=", 400)] {
        let r = raw_request(
            srv.addr,
            &format!("GET /v1/jobs?{query} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"),
        )
        .await;
        assert_eq!(status(&r), expected, "{query}: {r}");
        if expected == 200 {
            let list: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
            assert_eq!(list["next"], serde_json::Value::Null, "{r}");
            let jobs = list["jobs"].as_array().unwrap();
            assert!(jobs.len() <= 1, "{r}");
            if query == "key=build" {
                assert_eq!(jobs.len(), 1, "{r}");
                assert_eq!(jobs[0]["id"], created["id"], "{r}");
                assert_eq!(jobs[0]["spec"], created["spec"], "{r}");
            } else {
                assert!(jobs.is_empty(), "{r}");
            }
        } else {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(body_str(&r)).unwrap()["error"],
                "bad_request",
                "{r}"
            );
        }
    }

    let too_long = "k".repeat(lu_proto::MAX_IDEMPOTENCY_KEY_BYTES + 1);
    let r = raw_request(srv.addr, &format!("GET /v1/jobs?key={too_long} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 400, "{r}");

    // The lookup needs the same operator scope as the rest of the list route.
    let r = raw_request(
        srv.addr,
        "GET /v1/jobs?key=build HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&r), 401, "{r}");
    let r = raw_request(srv.addr, &format!("GET /v1/jobs?key=build HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 401, "{r}");

    let _ = srv.shutdown.send(());
}

fn heartbeat_json(node: &str) -> serde_json::Value {
    json!({"node": node, "harness": "idle", "providers": []})
}

async fn poll_with(addr: SocketAddr, token: &str, hb: &serde_json::Value) -> String {
    let body = json!({"heartbeat": hb, "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    raw_request(addr, &req).await
}

async fn assign_and_start(srv: &TestServer) -> (String, JobId, u64) {
    let r = submit_spec(srv.addr, &sample_spec(None)).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_owned();
    let job_id = JobId::parse(&id).unwrap();
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &heartbeat_json("node-a")).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let epoch = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap_or_else(|| panic!("no assignment: {events}"))["epoch"]
        .as_u64()
        .unwrap();
    let b = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    (id, job_id, epoch)
}

async fn cordons_for(addr: SocketAddr, node: &str) -> Vec<String> {
    let r = raw_request(addr, &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let view: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let n = view
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == node)
        .unwrap_or_else(|| panic!("node {node} missing: {view}"));
    n["cordon_details"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["provider"].as_str().map(str::to_owned))
        .collect()
}

async fn complete_as(
    srv: &TestServer,
    token: &str,
    id: &str,
    epoch: u64,
    outcome: serde_json::Value,
    retry_after_secs: Option<u64>,
) -> String {
    let mut comp = json!({"job_id": id, "epoch": epoch, "outcome": outcome});
    if let Some(secs) = retry_after_secs {
        comp["retry_after_secs"] = json!(secs);
    }
    let comp = comp.to_string();
    let req = format!("POST /v1/agent/jobs/{id}/complete HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", comp.len(), comp);
    raw_request(srv.addr, &req).await
}

async fn fail_provider(srv: &TestServer, retry_after_secs: Option<u64>) -> (String, u64) {
    let mut spec = sample_spec(None);
    spec.provider = Some("p1".to_owned());
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_owned();
    let healthy =
        json!({"node": "node-a", "harness": "idle", "providers": [{"name": "p1", "state": "ok"}]});
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &healthy).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap();
    assert_eq!(assign["job_id"], id);
    let epoch = assign["epoch"].as_u64().unwrap();
    let b = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let r = complete_as(
        srv,
        AGENT_A_TOKEN,
        &id,
        epoch,
        json!({"failed": "provider"}),
        retry_after_secs,
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    (id, epoch)
}

#[tokio::test]
async fn agent_happy_path_and_forbidden_cross_node_and_stale_epoch() {
    let srv = start_server().await;
    // create job
    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_string();

    // agent b token but heartbeat claims node-a -> forbidden
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_B_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 403, "{r}");

    // correct poll assigns
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let arr = events.as_array().unwrap();
    let assign = arr.iter().find(|e| e["kind"] == "assign").unwrap();
    assert_eq!(assign["job_id"], id);
    let epoch = assign["epoch"].as_u64().unwrap();

    // start
    let b = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    // renew
    let req = format!("POST /v1/agent/jobs/{id}/renew HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    assert!(body_str(&r).contains("\"cancel\""), "{r}");

    // log append
    let chunk = b"hello\n";
    let req = format!("POST /v1/agent/jobs/{id}/log?epoch={epoch}&offset=0 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", chunk.len());
    let mut packet = req.into_bytes();
    packet.extend_from_slice(chunk);
    let r = raw_request_bytes(srv.addr, &packet).await;
    let rs = String::from_utf8_lossy(&r).into_owned();
    assert_eq!(status(&rs), 200, "{rs}");

    // log offset conflict with length
    let req = format!("POST /v1/agent/jobs/{id}/log?epoch={epoch}&offset=0 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", chunk.len());
    let mut packet = req.into_bytes();
    packet.extend_from_slice(chunk);
    let r = raw_request_bytes(srv.addr, &packet).await;
    let rs = String::from_utf8_lossy(&r).into_owned();
    assert_eq!(status(&rs), 409, "{rs}");
    let jb: serde_json::Value = serde_json::from_str(body_str(&rs)).unwrap();
    assert!(jb.get("length").is_some(), "{rs}");

    // operator get logs
    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id}/logs?offset=0 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    assert!(body_str(&r).contains("hello"), "{r}");
    assert!(header_value(&r, "x-lu-log-length").is_some());

    // artifact upload report.json
    let rep = b"{}";
    let req = format!("PUT /v1/agent/jobs/{id}/artifacts/report.json?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", rep.len());
    let mut packet = req.into_bytes();
    packet.extend_from_slice(rep);
    let r = raw_request_bytes(srv.addr, &packet).await;
    let rs = String::from_utf8_lossy(&r).into_owned();
    assert_eq!(status(&rs), 200, "{rs}");

    // operator get artifact
    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id}/artifacts/report.json HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");

    // complete
    let comp = json!({"job_id": id, "epoch": epoch, "outcome": "done", "exit_code": 0}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/complete HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", comp.len(), comp);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    // stale epoch on renew (older epoch)
    let old = json!({"epoch": epoch.saturating_sub(1)}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/renew HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", old.len(), old);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 409, "{r}");
    let jb: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(jb["error"], "stale_epoch", "{r}");

    // nodes list and drain/undrain
    let r = raw_request(srv.addr, &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let r = raw_request(srv.addr, &format!("POST /v1/nodes/node-a/drain HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let r = raw_request(srv.addr, &format!("POST /v1/nodes/node-a/undrain HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");

    let _ = srv.shutdown.send(());
}

async fn finish_with_empty_diff(srv: &TestServer, allow_empty: bool) -> (String, String) {
    let mut spec = sample_spec(None);
    spec.allow_empty = allow_empty;
    let r = submit_spec(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_owned();

    let r = poll_with(srv.addr, AGENT_A_TOKEN, &heartbeat_json("node-a")).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap();
    assert_eq!(assign["job_id"], id);
    let epoch = assign["epoch"].as_u64().unwrap();
    let start = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", start.len(), start);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    let req = format!("PUT /v1/agent/jobs/{id}/artifacts/diff.patch?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: 0\r\n\r\n");
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    let r = complete_as(srv, AGENT_A_TOKEN, &id, epoch, json!("done"), None).await;
    assert_eq!(status(&r), 200, "{r}");
    (id, body_str(&r).to_owned())
}

async fn no_changes_metric(srv: &TestServer) -> String {
    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    body_str(&r)
        .lines()
        .find(|line| line.starts_with("lu_jobs_no_changes_total "))
        .unwrap_or_else(|| panic!("missing counter: {}", body_str(&r)))
        .to_owned()
}

async fn expect_failed_metric(srv: &TestServer) -> String {
    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    body_str(&r)
        .lines()
        .find(|line| line.starts_with("lu_jobs_expect_failed_total "))
        .unwrap_or_else(|| panic!("missing counter: {}", body_str(&r)))
        .to_owned()
}

async fn finish_with_expect_failure(srv: &TestServer) -> (String, String) {
    let r = submit_spec(srv.addr, &sample_spec(None)).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_owned();

    let r = poll_with(srv.addr, AGENT_A_TOKEN, &heartbeat_json("node-a")).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap();
    let epoch = assign["epoch"].as_u64().unwrap();

    let start = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", start.len(), start);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    let diff = "diff --git a/x.txt b/x.txt\n";
    let req = format!("PUT /v1/agent/jobs/{id}/artifacts/diff.patch?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n{}", diff.len(), diff);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    let comp =
        json!({"job_id": id, "epoch": epoch, "outcome": "done", "expect_failed": true}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/complete HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", comp.len(), comp);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    (id, body_str(&r).to_owned())
}

#[tokio::test]
async fn an_empty_diff_on_done_sets_the_note_and_counts_it() {
    let srv = start_server().await;
    let (id, completed) = finish_with_empty_diff(&srv, false).await;
    let view: serde_json::Value = serde_json::from_str(&completed).unwrap();
    assert_eq!(view["state"], "done", "{completed}");
    assert_eq!(view["outcome_note"], "no_changes", "{completed}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let view: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(view["outcome_note"], "no_changes", "{r}");
    assert_eq!(no_changes_metric(&srv).await, "lu_jobs_no_changes_total 1");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn allow_empty_suppresses_the_note_and_the_count() {
    let srv = start_server().await;
    let (id, completed) = finish_with_empty_diff(&srv, true).await;
    let view: serde_json::Value = serde_json::from_str(&completed).unwrap();
    assert!(view.get("outcome_note").is_none(), "{completed}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let view: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert!(view.get("outcome_note").is_none(), "{r}");
    assert_eq!(no_changes_metric(&srv).await, "lu_jobs_no_changes_total 0");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn an_expectation_failure_sets_the_note_and_counts_it() {
    let srv = start_server().await;
    let (id, completed) = finish_with_expect_failure(&srv).await;
    let view: serde_json::Value = serde_json::from_str(&completed).unwrap();
    assert_eq!(view["state"], "done", "{completed}");
    assert_eq!(view["outcome_note"], "expect_failed", "{completed}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let view: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(view["outcome_note"], "expect_failed", "{r}");
    assert_eq!(
        expect_failed_metric(&srv).await,
        "lu_jobs_expect_failed_total 1"
    );
    assert_eq!(no_changes_metric(&srv).await, "lu_jobs_no_changes_total 0");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn node_stats_are_served_bounded_to_operator_and_clear_on_next_heartbeat() {
    let srv = start_server().await;
    let stats_url = |node: &str| {
        format!("GET /v1/nodes/{node}/stats HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")
    };
    let zero_workspace_stats = || {
        json!({
            "workspace_bytes": 0,
            "workspace_count": 0,
            "workspace_reclaimed_bytes_total": 0,
            "workspace_evictions_total": 0,
            "workspace_delete_errors_total": 0
        })
    };

    // unknown node, before any heartbeat
    let r = raw_request(srv.addr, &stats_url("node-b")).await;
    assert_eq!(status(&r), 404, "{r}");

    // operator auth is required
    let r = raw_request(
        srv.addr,
        "GET /v1/nodes/node-a/stats HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&r), 401, "{r}");

    // a known node with no reported stats still reports zeroed workspace counters
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 204, "{r}");
    let r = raw_request(srv.addr, &stats_url("node-a")).await;
    assert_eq!(status(&r), 200, "{r}");
    let empty: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(empty, zero_workspace_stats(), "{r}");

    let job_id = JobId::new().to_string();
    let heartbeat = json!({
        "node": "node-a",
        "harness": "busy",
        "version": "0.1.10",
        "providers": [],
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
            "job_id": job_id,
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
                "cmdline": "opencode run",
                "open_files": 3
            }]
        }]
    });
    let poll_body = json!({"heartbeat": heartbeat, "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 204, "{r}");

    let r = raw_request(srv.addr, &stats_url("node-a")).await;
    assert_eq!(status(&r), 200, "{r}");
    let stats: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(stats["host"]["load1"], 0.5, "{r}");
    assert_eq!(stats["jobs"][0]["job_id"], job_id, "{r}");
    assert_eq!(stats["jobs"][0]["stuck"], true, "{r}");
    assert_eq!(stats["jobs"][0]["processes"][0]["pid"], 4242, "{r}");

    let r = raw_request(srv.addr, &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let nodes: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let node = nodes
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "node-a")
        .unwrap();
    assert_eq!(node["version"], "0.1.10", "{r}");
    assert_eq!(node["load1"], 0.5, "{r}");
    assert_eq!(node["mem_available_kb"], 4096, "{r}");
    assert_eq!(node["mem_total_kb"], 8192, "{r}");
    assert_eq!(node["stuck"], 1, "{r}");

    // the next heartbeat replaces, rather than merges, the previous stats
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 204, "{r}");
    let r = raw_request(srv.addr, &stats_url("node-a")).await;
    assert_eq!(status(&r), 200, "{r}");
    let cleared: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(cleared, zero_workspace_stats(), "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn long_poll_times_out_and_wakes_on_job_creation() {
    let srv = start_server().await;
    // timeout with 204
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 1}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 204, "{r}");

    // wake on job creation
    let addr = srv.addr;
    let poll_task = tokio::spawn(async move {
        let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 5}).to_string();
        let req = format!(
            "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            poll_body.len(), poll_body
        );
        raw_request(addr, &req).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let poll_resp = tokio::time::timeout(Duration::from_secs(3), poll_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status(&poll_resp), 200, "{poll_resp}");
    assert!(body_str(&poll_resp).contains("\"assign\""), "{poll_resp}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn cancel_while_waiting_long_poll() {
    let srv = start_server().await;
    // create and assign job to node-a
    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_string();
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let _ = raw_request(srv.addr, &req).await; // assigned

    // start a waiting poll that should be woken by cancel
    let addr = srv.addr;
    let poll_task = tokio::spawn(async move {
        let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 5}).to_string();
        let req = format!(
            "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            poll_body.len(), poll_body
        );
        raw_request(addr, &req).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = raw_request(srv.addr, &format!("POST /v1/jobs/{id}/cancel HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let poll_resp = tokio::time::timeout(Duration::from_secs(3), poll_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status(&poll_resp), 200, "{poll_resp}");
    assert!(body_str(&poll_resp).contains("\"cancel\""), "{poll_resp}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn oversize_bodies_refused_and_error_bodies_have_no_control_chars() {
    let srv = start_server().await;
    // oversize JSON body (>1MiB) create job
    let big_task = "a".repeat(1024 * 1024 + 100);
    let spec = json!({
        "source": {"repo": "https://example.net/repo.git", "ref": "main"},
        "task": big_task,
        "harness":"opencode",
        "timeout_secs": 3600
    })
    .to_string();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        spec.len(), spec
    );
    let r = raw_request(srv.addr, &req).await;
    assert!(status(&r) == 413 || status(&r) == 400, "{r}");
    let b = body_str(&r);
    assert!(
        !b.chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t'),
        "{b:?}"
    );

    // invalid JSON with control char in field value? ensure error message sanitised - send malformed JSON
    let bad = "{\"repo\":\"https://example.net/r.git\",\"git_ref\":\"main\",\"task\":\"x\",\"harness\":\"opencode\",\"timeout_secs\":3600,\"oops\u{0007}\":1}";
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        bad.len(),
        bad
    );
    let r = raw_request(srv.addr, &req).await;
    assert!(status(&r) >= 400, "{r}");
    let b = body_str(&r);
    assert!(b.contains("\"error\""), "{b}");
    assert!(
        !b.chars().any(|c| c == '\u{0007}' || c == '\u{001b}'),
        "{b:?}"
    );

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn graceful_shutdown_ends_waiting_polls() {
    let srv = start_server().await;
    let addr = srv.addr;
    let poll_task = tokio::spawn(async move {
        let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 30}).to_string();
        let req = format!(
            "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            poll_body.len(), poll_body
        );
        raw_request(addr, &req).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = srv.shutdown.send(());
    let res = tokio::time::timeout(Duration::from_secs(3), poll_task).await;
    assert!(res.is_ok(), "waiting poll should end on shutdown");
    let resp = res.unwrap().unwrap();
    // server may end connection or return 204/empty response depending on shutdown timing; key is no leak/hang.
    // If it returned a response, it should be valid HTTP status or empty due to connection close.
    let _ = resp;
}

fn local_repo(path: &Path) -> (String, String, String) {
    use git2::IndexAddOption;

    let repo = common::init_main_repo(path);
    let file = path.join("file.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = Signature::now("lu", "lu@example.net").unwrap();
    let first = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "first",
            &tree,
            &[],
        )
        .unwrap();
    let first_commit = repo.find_commit(first).unwrap();
    repo.tag_lightweight("v1", first_commit.as_object(), false)
        .unwrap();
    std::fs::write(&file, "two\n").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let second = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "second",
            &tree,
            &[&first_commit],
        )
        .unwrap();
    (
        format!("file://{}", path.display()),
        first.to_string(),
        second.to_string(),
    )
}

fn repo_entry(url: &str) -> ControllerRepo {
    ControllerRepo {
        url: url.to_owned(),
        name: "local".to_owned(),
        credential_file: None,
        refs: vec!["*".to_owned()],
        allow_file_urls: true,
    }
}

fn operator_get(path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"
    )
}

fn operator_post(path: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n"
    )
}

async fn post_job(addr: SocketAddr, spec: &JobSpec) -> String {
    let body = serde_json::to_string(spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    raw_request(addr, &req).await
}

#[tokio::test]
async fn dependency_views_and_validation() {
    let srv = start_server().await;

    let r = post_job(srv.addr, &sample_spec(None)).await;
    assert_eq!(status(&r), 201, "{r}");
    let a: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let a_id = a["id"].as_str().unwrap().to_string();
    assert_eq!(a["state"], "queued");
    assert_eq!(a["after"], json!([]));
    assert_eq!(a["waiting_on"], json!([]));
    let dep = JobId::parse(&a_id).unwrap();

    let mut spec_b = sample_spec(None);
    spec_b.after = vec![dep.clone()];
    let r = post_job(srv.addr, &spec_b).await;
    assert_eq!(status(&r), 201, "{r}");
    let b: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let b_id = b["id"].as_str().unwrap().to_string();
    assert_eq!(b["state"], "blocked");
    assert_eq!(b["after"], json!([a_id]));
    assert_eq!(b["waiting_on"], json!([a_id]));

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{b_id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let got: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(got["state"], "blocked");
    assert_eq!(got["after"], json!([a_id]));
    assert_eq!(got["waiting_on"], json!([a_id]));

    let r = raw_request(srv.addr, &format!("GET /v1/jobs?state=blocked HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let list: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let listed = list["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == b_id)
        .expect("blocked job should be listed");
    assert_eq!(listed["waiting_on"], json!([a_id]));
    assert_eq!(listed["after"], json!([a_id]));

    // Cancelling a blocked job is allowed, like a queued one.
    let r = raw_request(srv.addr, &format!("POST /v1/jobs/{b_id}/cancel HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let cancelled: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(cancelled["state"], "cancelled");

    // An unknown dependency is a 422 naming the id.
    let missing = JobId::new();
    let mut unknown = sample_spec(None);
    unknown.after = vec![missing.clone()];
    let r = post_job(srv.addr, &unknown).await;
    assert_eq!(status(&r), 422, "{r}");
    let err: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(err["error"], "unprocessable", "{r}");
    assert!(
        err["message"].as_str().unwrap().contains(missing.as_str()),
        "{r}"
    );

    // Spec validation failures are also 422s.
    let mut too_many = sample_spec(None);
    too_many.after = (0..=MAX_AFTER_JOBS).map(|_| JobId::new()).collect();
    let r = post_job(srv.addr, &too_many).await;
    assert_eq!(status(&r), 422, "{r}");
    let err: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert!(err["message"].as_str().unwrap().contains("after"), "{r}");

    let mut duplicate = sample_spec(None);
    duplicate.after = vec![dep.clone(), dep];
    let r = post_job(srv.addr, &duplicate).await;
    assert_eq!(status(&r), 422, "{r}");
    let err: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert!(
        err["message"].as_str().unwrap().contains("duplicate"),
        "{r}"
    );
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn agent_dependency_routes_list_and_serve_diffs() {
    let srv = start_server().await;

    // Finish a dependency so the dependent is runnable.
    let r = post_job(srv.addr, &sample_spec(None)).await;
    assert_eq!(status(&r), 201, "{r}");
    let dep: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let dep_id = dep["id"].as_str().unwrap().to_string();

    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let dep_epoch = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign" && e["job_id"] == dep_id)
        .and_then(|e| e["epoch"].as_u64())
        .expect("dependency should be assigned");

    let b = json!({"epoch": dep_epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{dep_id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let comp = json!({"job_id": dep_id, "epoch": dep_epoch, "outcome": "done", "exit_code": 0})
        .to_string();
    let req = format!("POST /v1/agent/jobs/{dep_id}/complete HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", comp.len(), comp);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    let mut spec = sample_spec(None);
    spec.after = vec![JobId::parse(&dep_id).unwrap()];
    let r = post_job(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let job_id = job["id"].as_str().unwrap().to_string();
    assert_eq!(job["state"], "queued", "{r}");

    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let epoch = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign" && e["job_id"] == job_id)
        .and_then(|e| e["epoch"].as_u64())
        .expect("dependent should be assigned");

    let deps = |token: &str, epoch: u64| {
        format!("GET /v1/agent/jobs/{job_id}/deps?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n")
    };
    let diff = |token: &str, epoch: u64| {
        format!("GET /v1/agent/jobs/{job_id}/deps/{dep_id}/diff?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n")
    };

    // No diff stored yet: the list reports zero bytes and the diff is 404.
    let r = raw_request(srv.addr, &deps(AGENT_A_TOKEN, epoch)).await;
    assert_eq!(status(&r), 200, "{r}");
    let list: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["job_id"], dep_id);
    assert_eq!(list[0]["state"], "done");
    assert_eq!(list[0]["diff_bytes"], 0);
    assert_eq!(list[0]["failure_class"], serde_json::Value::Null);
    assert_eq!(list[0]["node"], "node-a");
    let r = raw_request(srv.addr, &diff(AGENT_A_TOKEN, epoch)).await;
    assert_eq!(status(&r), 404, "{r}");

    let patch = b"--- a/README.md\n+++ b/README.md\n";
    let dep_dir = srv._artifacts_dir.join(&dep_id);
    std::fs::create_dir_all(&dep_dir).unwrap();
    std::fs::write(dep_dir.join("diff.patch"), patch).unwrap();

    let r = raw_request(srv.addr, &deps(AGENT_A_TOKEN, epoch)).await;
    assert_eq!(status(&r), 200, "{r}");
    let list: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(list[0]["diff_bytes"], patch.len());

    let r = raw_request(srv.addr, &diff(AGENT_A_TOKEN, epoch)).await;
    assert_eq!(status(&r), 200, "{r}");
    assert_eq!(body_str(&r).as_bytes(), patch);

    // The artifact route serves only the three fixed names and honours `tail`.
    let other = JobId::new();
    let artifact = |token: &str, epoch: u64, name: &str, tail: &str| {
        format!("GET /v1/agent/jobs/{job_id}/deps/{dep_id}/artifacts/{name}?epoch={epoch}{tail} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n")
    };
    let r = raw_request(srv.addr, &artifact(AGENT_A_TOKEN, epoch, "report.json", "")).await;
    assert_eq!(status(&r), 404, "{r}");
    std::fs::write(dep_dir.join("report.json"), b"{\"summary\":\"ok\"}").unwrap();
    std::fs::write(dep_dir.join("run.log"), b"tool output").unwrap();
    let r = raw_request(srv.addr, &artifact(AGENT_A_TOKEN, epoch, "report.json", "")).await;
    assert_eq!(status(&r), 200, "{r}");
    assert_eq!(body_str(&r), "{\"summary\":\"ok\"}");
    let r = raw_request(
        srv.addr,
        &artifact(AGENT_A_TOKEN, epoch, "run.log", "&tail=4"),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    assert_eq!(body_str(&r), "tput");

    // A large log is seeked to its tail rather than read into memory, and a file above the
    // documented cap is refused even for a tail request.
    let mut big = vec![b'x'; 1024 * 1024];
    big.extend_from_slice(b"0123456789abcdef");
    std::fs::write(dep_dir.join("run.log"), &big).unwrap();
    let r = raw_request(
        srv.addr,
        &artifact(AGENT_A_TOKEN, epoch, "run.log", "&tail=16"),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    assert_eq!(body_str(&r), "0123456789abcdef");
    std::fs::File::create(dep_dir.join("run.log"))
        .unwrap()
        .set_len(MAX_LOG_BYTES + 1)
        .unwrap();
    let r = raw_request(
        srv.addr,
        &artifact(AGENT_A_TOKEN, epoch, "run.log", "&tail=16"),
    )
    .await;
    assert_eq!(status(&r), 413, "{r}");

    assert_eq!(
        status(&raw_request(srv.addr, &artifact(AGENT_A_TOKEN, epoch, "evil.txt", ""),).await),
        404
    );
    let req = format!("GET /v1/agent/jobs/{job_id}/deps/{other}/artifacts/diff.patch?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\n\r\n");
    assert_eq!(status(&raw_request(srv.addr, &req).await), 403);

    // Wrong scope, wrong node and epoch fencing are refused.
    assert_eq!(
        status(&raw_request(srv.addr, &deps(OP_TOKEN, epoch)).await),
        401
    );
    assert_eq!(
        status(&raw_request(srv.addr, &deps(AGENT_B_TOKEN, epoch)).await),
        403
    );
    assert_eq!(
        status(&raw_request(srv.addr, &deps(AGENT_A_TOKEN, epoch + 1)).await),
        409
    );
    assert_eq!(
        status(&raw_request(srv.addr, &diff(AGENT_B_TOKEN, epoch)).await),
        403
    );

    // A job that is not in `after` is not readable.
    let req = format!("GET /v1/agent/jobs/{job_id}/deps/{other}/diff?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\n\r\n");
    assert_eq!(status(&raw_request(srv.addr, &req).await), 403);

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn an_older_agent_sees_no_new_fields_and_reports_old_classes() {
    let srv = start_server().await;
    let r = post_job(srv.addr, &sample_spec(None)).await;
    assert_eq!(status(&r), 201, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = job["id"].as_str().unwrap().to_string();

    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign")
        .expect("job should be assigned");
    let epoch = assign["epoch"].as_u64().unwrap();
    assert!(
        assign["spec"].get("stack_dependencies").is_none(),
        "removed field must not appear on the wire: {assign}"
    );

    let b = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let comp =
        json!({"job_id": id, "epoch": epoch, "outcome": {"failed": "execution"}, "exit_code": 1})
            .to_string();
    let req = format!("POST /v1/agent/jobs/{id}/complete HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", comp.len(), comp);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    let r = raw_request(srv.addr, &format!("GET /v1/jobs/{id} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    let got: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(got["failure_class"], "execution", "{r}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn drain_and_undrain_an_unknown_node_is_not_found() {
    let srv = start_server().await;
    for action in ["drain", "undrain"] {
        let r = raw_request(
            srv.addr,
            &format!("POST /v1/nodes/ghost/{action} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n"),
        )
        .await;
        assert_eq!(status(&r), 404, "{r} {action}");
        let jb: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
        assert_eq!(jb["error"], "not_found", "{r}");
    }
    // nothing was persisted for the unknown node
    let r = raw_request(
        srv.addr,
        &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"),
    )
    .await;
    let list: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert!(list.as_array().unwrap().is_empty(), "{r}");

    // the same node becomes drainable once it has heartbeated
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    // no jobs are queued, so the poll heartbeats and reports nothing to do
    assert_eq!(status(&r), 204, "{r}");
    let r = raw_request(
        srv.addr,
        &format!("POST /v1/nodes/node-a/drain HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n"),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn uncordon_requires_operator_scope_and_a_known_node() {
    let srv = start_server().await;
    for auth in ["", &format!("Authorization: Bearer {AGENT_A_TOKEN}\r\n")] {
        let r = raw_request(
            srv.addr,
            &format!("POST /v1/nodes/node-a/uncordon HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{auth}Content-Length: 0\r\n\r\n"),
        )
        .await;
        assert_eq!(status(&r), 401, "{r}");
    }
    let r = raw_request(
        srv.addr,
        &format!("POST /v1/nodes/ghost/uncordon HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n"),
    )
    .await;
    assert_eq!(status(&r), 404, "{r}");
    let jb: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(jb["error"], "not_found", "{r}");

    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    assert_eq!(status(&raw_request(srv.addr, &req).await), 204);
    let r = raw_request(
        srv.addr,
        &format!("POST /v1/nodes/node-a/uncordon HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n"),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    let jb: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(jb["removed"], json!([]), "{r}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn node_views_expose_cordon_details_and_uncordon_clears_them() {
    let srv = start_server().await;
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    assert_eq!(status(&raw_request(srv.addr, &req).await), 204);
    let node = NodeName::parse("node-a").unwrap();
    srv.store
        .set_cordon(
            &node,
            "p1",
            CordonSource::Failure,
            "Provider p1 failed: timeout",
            1_700_000_000,
            120_000,
        )
        .unwrap();
    srv.store
        .set_cordon(
            &node,
            "p2",
            CordonSource::Reiki,
            "Reiki cooldown for p2",
            1_700_000_001,
            120_000,
        )
        .unwrap();

    let nodes = node_list(srv.addr).await;
    let detail = &nodes[0]["cordon_details"];
    assert_eq!(detail[0]["provider"], "p1");
    assert_eq!(detail[0]["source"], "failure");
    assert_eq!(detail[0]["reason"], "Provider p1 failed: timeout");
    assert_eq!(detail[0]["set_at"], 1_700_000_000);
    assert!(detail[0]["until"].as_i64().is_some());
    assert_eq!(detail[1]["source"], "reiki");
    assert_eq!(detail[1]["until"], serde_json::Value::Null);
    assert_eq!(detail[1]["lifts_after_healthy_heartbeats"], 2);

    let body = r#"{"provider":"p1"}"#;
    let r = raw_request(
        srv.addr,
        &format!("POST /v1/nodes/node-a/uncordon HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    let jb: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(jb["removed"], json!(["p1"]), "{r}");
    let nodes = node_list(srv.addr).await;
    assert_eq!(nodes[0]["cordon_details"].as_array().unwrap().len(), 1);

    let r = raw_request(
        srv.addr,
        &format!("POST /v1/nodes/node-a/uncordon HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Length: 0\r\n\r\n"),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    let jb: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(jb["removed"], json!(["p2"]), "{r}");
    let nodes = node_list(srv.addr).await;
    assert!(nodes[0]["cordon_details"].as_array().unwrap().is_empty());

    let r = raw_request(
        srv.addr,
        &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    // Cordons were cleared, so the HELP line remains but no series is emitted.
    let text = body_str(&r);
    assert!(text.contains("lu_node_cordon_seconds_remaining"), "{text}");
    let _ = srv.shutdown.send(());
}

async fn node_list(addr: SocketAddr) -> serde_json::Value {
    let r = raw_request(
        addr,
        &format!("GET /v1/nodes HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"),
    )
    .await;
    assert_eq!(status(&r), 200, "{r}");
    serde_json::from_str::<serde_json::Value>(body_str(&r)).unwrap()
}

#[tokio::test]
async fn status_requires_operator_token_and_reports_counts() {
    let srv = start_server().await;
    let r = raw_request(
        srv.addr,
        "GET /v1/status HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&r), 401, "{r}");

    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");

    let r = raw_request(srv.addr, &format!("GET /v1/status HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let body = body_str(&r);
    let status: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"), "{body}");
    assert_eq!(status["schema_version"], lu_store::SCHEMA_VERSION, "{body}");
    assert_eq!(status["jobs"]["queued"], 1, "{body}");
    assert_eq!(status["jobs"]["done"], 0, "{body}");
    assert_eq!(status["nodes"], 0, "{body}");
    assert_eq!(status["nodes_online"], 0, "{body}");
    assert_eq!(status["lease_secs"], 6, "{body}");
    assert_eq!(status["recovered_on_boot"], 0, "{body}");
    assert!(status["uptime_secs"].is_u64(), "{body}");
    assert!(status["db_bytes"].is_u64(), "{body}");
    assert!(status["oldest_queued_secs"].is_u64(), "{body}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn metrics_requires_operator_token() {
    let srv = start_server().await;
    // no token -> 401
    let r = raw_request(
        srv.addr,
        "GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&r), 401, "{r}");
    // agent token on operator route -> 401
    let r = raw_request(
        srv.addr,
        &format!(
            "GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(status(&r), 401, "{r}");
    // valid operator -> 200
    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    assert!(header_value(&r, "content-type")
        .unwrap_or("")
        .starts_with("text/plain"));
    assert!(body_str(&r).contains("lu_controller_up 1"), "{r}");
    // empty system: zero-filled gauges
    for st in [
        "queued",
        "leased",
        "running",
        "recovering",
        "needs_input",
        "done",
        "failed",
        "cancelled",
    ] {
        assert!(
            body_str(&r).contains(&format!("lu_jobs{{state=\"{st}\"}} 0")),
            "{r}"
        );
    }
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn metrics_series_after_job_lifecycle() {
    let srv = start_server().await;
    // submit a job
    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_string();

    // no agents polling: job stays queued
    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let b = body_str(&r);
    assert!(b.contains("lu_jobs{state=\"queued\"} 1"), "{b}");
    assert!(b.contains("lu_nodes 0"), "{b}");
    assert!(b.contains("lu_nodes_online 0"), "{b}");
    assert!(b.contains("lu_nodes_drained 0"), "{b}");
    assert!(b.contains("lu_controller_up 1"), "{b}");
    assert!(
        b.contains(&format!(
            "lu_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )),
        "{b}"
    );
    assert!(
        b.contains(&format!("lu_schema_version {}", lu_store::SCHEMA_VERSION)),
        "{b}"
    );
    assert!(b.contains("lu_recovered_on_boot 0"), "{b}");
    assert!(b.contains("lu_queue_oldest_seconds "), "{b}");
    assert!(
        b.lines().any(|line| line.starts_with("lu_uptime_seconds ")),
        "{b}"
    );
    assert!(
        b.lines().any(|line| line.starts_with("lu_db_bytes ")),
        "{b}"
    );
    assert!(
        b.contains("# HELP lu_jobs_completed_total Total number of jobs completed per outcome"),
        "{b}"
    );
    assert!(b.contains("# TYPE lu_jobs_completed_total counter"), "{b}");
    assert!(
        b.contains("lu_jobs_completed_total{outcome=\"done\"} 0"),
        "{b}"
    );
    assert!(
        b.contains("lu_jobs_completed_total{outcome=\"failed\"} 0"),
        "{b}"
    );
    assert!(
        b.contains("lu_jobs_completed_total{outcome=\"cancelled\"} 0"),
        "{b}"
    );

    // register a heartbeat with providers incl. escaping-provoked names later
    let poll_body = serde_json::to_string(&serde_json::json!({"heartbeat": {"node": "node-a", "harness": "idle", "providers": [{"name": "prov-a", "state": "ok"}], "host": {"load1": 0.5, "load5": 0.25, "load15": 0.1, "mem_total_kb": 100, "mem_available_kb": 50, "swap_used_kb": 0, "disk_free_bytes": 250, "disk_total_bytes": 1000, "uptime_secs": 10}}, "wait_secs": 0})).unwrap();
    let r = raw_request(srv.addr, &format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    )).await;
    assert_eq!(status(&r), 200, "{r}");
    let poll_resp = r.clone();

    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    let b = body_str(&r);
    assert!(b.contains("lu_nodes 1"), "{b}");
    assert!(b.contains("lu_nodes_online 1"), "{b}");
    assert!(b.contains("lu_nodes_drained 0"), "{b}");
    assert!(
        b.contains("lu_node_providers{node=\"node-a\",provider=\"prov-a\",state=\"ok\"} 1"),
        "{b}"
    );
    assert!(
        b.contains("lu_node_disk_free_percent{node=\"node-a\"} 25"),
        "{b}"
    );
    assert!(
        b.lines()
            .any(|line| line.starts_with("lu_node_last_seen_seconds{node=\"node-a\"} ")),
        "{b}"
    );

    // assign + start + complete done -> counter updates, state moves to done
    let events: serde_json::Value = serde_json::from_str(body_str(&poll_resp)).unwrap();
    let arr = events.as_array().unwrap();
    let assign = arr.iter().find(|e| e["kind"] == "assign").unwrap();
    let epoch = assign["epoch"].as_u64().unwrap();
    let start_body = format!("{{\"epoch\": {epoch}}}");
    let r = raw_request(srv.addr, &format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", start_body.len(), start_body)).await;
    assert_eq!(status(&r), 200, "{r}");

    let comp = serde_json::to_string(
        &serde_json::json!({"job_id": id, "epoch": epoch, "outcome": "done", "exit_code": 0}),
    )
    .unwrap();
    let r = raw_request(srv.addr, &format!("POST /v1/agent/jobs/{id}/complete HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", comp.len(), comp)).await;
    assert_eq!(status(&r), 200, "{r}");

    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    let b = body_str(&r);
    assert!(b.contains("lu_jobs{state=\"queued\"} 0"), "{b}");
    assert!(b.contains("lu_jobs{state=\"done\"} 1"), "{b}");
    assert!(
        b.contains("lu_jobs_completed_total{outcome=\"done\"} 1"),
        "{b}"
    );

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn stuck_stopped_metric_counts_cancel_and_retry() {
    let srv = start_server().await;
    assert_eq!(srv.state.stuck_stopped_total(StuckPolicy::Cancel), 0);
    assert_eq!(srv.state.stuck_stopped_total(StuckPolicy::Retry), 0);
    srv.state.count_stuck_stopped(Some(StuckPolicy::Cancel));
    srv.state.count_stuck_stopped(Some(StuckPolicy::Retry));
    srv.state.count_stuck_stopped(Some(StuckPolicy::Retry));
    srv.state.count_stuck_stopped(Some(StuckPolicy::None));

    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let b = body_str(&r);
    assert!(
        b.contains("# HELP lu_jobs_stuck_stopped_total Jobs stopped by the agent's stuck policy"),
        "{b}"
    );
    assert!(
        b.contains("lu_jobs_stuck_stopped_total{mode=\"cancel\"} 1"),
        "{b}"
    );
    assert!(
        b.contains("lu_jobs_stuck_stopped_total{mode=\"retry\"} 2"),
        "{b}"
    );
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn metrics_text_format_parses() {
    let srv = start_server().await;
    let poll_body = serde_json::to_string(&serde_json::json!({"heartbeat": {"node": "node-a", "harness": "idle", "providers": []}, "wait_secs": 0})).unwrap();
    let r = raw_request(srv.addr, &format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    )).await;
    assert!(status(&r) == 200 || status(&r) == 204, "{r}");

    let r = raw_request(srv.addr, &format!("GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")).await;
    assert_eq!(status(&r), 200, "{r}");
    let b = body_str(&r);

    // body must be valid Prometheus text format: each line either a comment or `metric{...} value`
    // or `metric value` (no labels).
    for line in b.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (metric_part, value_part) = match line.split_once(' ') {
            Some((m, v)) => (m, v.trim()),
            None => panic!("malformed line (no value): {line:?}"),
        };
        if metric_part.contains('{') {
            assert!(
                metric_part.ends_with('}'),
                "metric line has unbalanced braces: {line:?}"
            );
        }
        assert!(
            value_part.parse::<i64>().map(|v| v >= 0).unwrap_or(false),
            "non-numeric value in: {line:?}"
        );
    }
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn stale_epoch_artifact_upload_leaves_the_file_alone() {
    let srv = start_server().await;
    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_string();

    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let events = events.as_array().unwrap().clone();
    let assign = events
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap()
        .clone();
    let epoch = assign["epoch"].as_u64().unwrap();

    let rep = b"original-report";
    let req = format!("PUT /v1/agent/jobs/{id}/artifacts/report.json?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", rep.len());
    let mut packet = req.into_bytes();
    packet.extend_from_slice(rep);
    let r = raw_request_bytes(srv.addr, &packet).await;
    let rs = String::from_utf8_lossy(&r).into_owned();
    assert_eq!(status(&rs), 200, "{rs}");

    let path = srv._artifacts_dir.join(&id).join("report.json");
    assert_eq!(std::fs::read(&path).unwrap(), rep);

    // an agent at an old epoch uploads; the stored artifact must survive
    let stale = epoch.saturating_sub(1);
    let new = b"clobbered-by-stale-agent";
    let req = format!("PUT /v1/agent/jobs/{id}/artifacts/report.json?epoch={stale} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", new.len());
    let mut packet = req.into_bytes();
    packet.extend_from_slice(new);
    let r = raw_request_bytes(srv.addr, &packet).await;
    let rs = String::from_utf8_lossy(&r).into_owned();
    assert_eq!(status(&rs), 409, "{rs}");
    let jb: serde_json::Value = serde_json::from_str(body_str(&rs)).unwrap();
    assert_eq!(jb["error"], "stale_epoch", "{rs}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        rep,
        "stale epoch must not overwrite the file"
    );
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn log_append_100_bytes_below_the_cap_keeps_the_marker() {
    let srv = start_server().await;
    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_string();
    let job_id = JobId::parse(&id).unwrap();

    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let events = events.as_array().unwrap().clone();
    let assign = events
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap()
        .clone();
    let epoch = assign["epoch"].as_u64().unwrap();
    let b = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    // seed the stored log length to 100 bytes under the cap, with the same bytes on disk
    let seeded = MAX_LOG_BYTES - 100;
    let job_dir = srv._artifacts_dir.join(&id);
    std::fs::create_dir_all(&job_dir).unwrap();
    std::fs::write(job_dir.join("run.log"), vec![b'l'; seeded as usize]).unwrap();
    srv.store.reserve_log(&job_id, epoch, 0, seeded).unwrap();

    // a chunk larger than the 100 bytes that remain
    let chunk = vec![b'a'; 4096];
    let req = format!("POST /v1/agent/jobs/{id}/log?epoch={epoch}&offset={seeded} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", chunk.len());
    let mut packet = req.into_bytes();
    packet.extend_from_slice(&chunk);
    let r = raw_request_bytes(srv.addr, &packet).await;
    let rs = String::from_utf8_lossy(&r).into_owned();
    assert_eq!(status(&rs), 200, "{rs}");

    // the stored log ends with the marker and never exceeds the cap
    let stored = std::fs::read(job_dir.join("run.log")).unwrap();
    assert!(
        stored.ends_with(b"\n[log truncated]\n"),
        "stored log lacks the truncation marker, tail: {:?}",
        &stored[stored.len().saturating_sub(32)..]
    );
    assert!(
        stored.len() as u64 <= MAX_LOG_BYTES,
        "stored log exceeds the cap"
    );
    let rec = srv.store.get_job(&job_id).unwrap();
    assert_eq!(rec.log_len, MAX_LOG_BYTES);
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn reiki_cordon_yields_to_two_healthy_heartbeats_and_flagless_does_not() {
    let srv = start_server().await;
    let healthy =
        json!({"node": "node-a", "harness": "idle", "providers": [{"name": "p1", "state": "ok"}]});

    let (id, _) = fail_provider(&srv, Some(3600)).await;
    assert_eq!(cordons_for(srv.addr, "node-a").await, vec!["p1"]);
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &healthy).await;
    assert_eq!(status(&r), 204, "{r}");
    assert_eq!(cordons_for(srv.addr, "node-a").await, vec!["p1"]);
    let r = poll_with(srv.addr, AGENT_A_TOKEN, &healthy).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap_or_else(|| panic!("no assignment after two healthy heartbeats: {events}"));
    assert_eq!(assign["job_id"], id);
    assert!(cordons_for(srv.addr, "node-a").await.is_empty());
    let epoch = assign["epoch"].as_u64().unwrap();
    let b = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let r = complete_as(&srv, AGENT_A_TOKEN, &id, epoch, json!("done"), None).await;
    assert_eq!(status(&r), 200, "{r}");

    let (id, _) = fail_provider(&srv, None).await;
    assert_eq!(cordons_for(srv.addr, "node-a").await, vec!["p1"]);
    for _ in 0..2 {
        let r = poll_with(srv.addr, AGENT_A_TOKEN, &healthy).await;
        assert_eq!(status(&r), 204, "{r}");
    }
    assert_eq!(cordons_for(srv.addr, "node-a").await, vec!["p1"]);
    let rec = srv.store.get_job(&JobId::parse(&id).unwrap()).unwrap();
    assert_eq!(rec.state, lu_proto::JobState::Queued);
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn log_append_with_less_room_than_the_marker_keeps_the_marker() {
    let srv = start_server().await;
    let spec = sample_spec(None);
    let body = serde_json::to_string(&spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_string();
    let job_id = JobId::parse(&id).unwrap();

    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let events = events.as_array().unwrap().clone();
    let assign = events
        .iter()
        .find(|e| e["kind"] == "assign")
        .unwrap()
        .clone();
    let epoch = assign["epoch"].as_u64().unwrap();
    let b = json!({"epoch": epoch}).to_string();
    let req = format!("POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b);
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");

    // seed the stored log length to 20 bytes under the cap, 3 bytes of room before the marker, with the same bytes on disk
    let seeded = MAX_LOG_BYTES - 20;
    let job_dir = srv._artifacts_dir.join(&id);
    std::fs::create_dir_all(&job_dir).unwrap();
    std::fs::write(job_dir.join("run.log"), vec![b'l'; seeded as usize]).unwrap();
    srv.store.reserve_log(&job_id, epoch, 0, seeded).unwrap();

    // a chunk larger than the room that remains
    let chunk = vec![b'a'; 4096];
    let req = format!("POST /v1/agent/jobs/{id}/log?epoch={epoch}&offset={seeded} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", chunk.len());
    let mut packet = req.into_bytes();
    packet.extend_from_slice(&chunk);
    let r = raw_request_bytes(srv.addr, &packet).await;
    let rs = String::from_utf8_lossy(&r).into_owned();
    assert_eq!(status(&rs), 200, "{rs}");

    // the stored log ends with the marker and never exceeds the cap
    let stored = std::fs::read(job_dir.join("run.log")).unwrap();
    assert!(
        stored.ends_with(b"\n[log truncated]\n"),
        "stored log lacks the truncation marker, tail: {:?}",
        &stored[stored.len().saturating_sub(32)..]
    );
    assert!(
        stored.len() as u64 <= MAX_LOG_BYTES,
        "stored log exceeds the cap"
    );
    let rec = srv.store.get_job(&job_id).unwrap();
    assert_eq!(rec.log_len, MAX_LOG_BYTES);
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn failed_log_write_rolls_back_the_reservation() {
    let srv = start_server().await;
    let (id, job_id, epoch) = assign_and_start(&srv).await;
    let job_dir = srv._artifacts_dir.join(&id);
    std::fs::create_dir_all(&job_dir).unwrap();
    let log_path = job_dir.join("run.log");
    // A directory at the log path makes the write fail after the store reserved the bytes.
    std::fs::create_dir(&log_path).unwrap();

    let chunk = b"hello\n";
    let append = format!("POST /v1/agent/jobs/{id}/log?epoch={epoch}&offset=0 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n", chunk.len());
    let mut packet = append.into_bytes();
    packet.extend_from_slice(chunk);
    let r = String::from_utf8_lossy(&raw_request_bytes(srv.addr, &packet).await).into_owned();
    assert_eq!(status(&r), 500, "{r}");
    assert_eq!(srv.store.get_job(&job_id).unwrap().log_len, 0);

    // The retry at the same offset succeeds and the log has exactly the retried bytes.
    std::fs::remove_dir(&log_path).unwrap();
    let r = String::from_utf8_lossy(&raw_request_bytes(srv.addr, &packet).await).into_owned();
    assert_eq!(status(&r), 200, "{r}");
    assert_eq!(
        srv.store.get_job(&job_id).unwrap().log_len,
        chunk.len() as u64
    );
    assert_eq!(std::fs::read(&log_path).unwrap(), chunk);
    let logs = raw_request(
        srv.addr,
        &format!("GET /v1/jobs/{id}/logs?offset=0 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"),
    )
    .await;
    assert_eq!(body_str(&logs), "hello\n", "{logs}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn get_logs_stops_at_the_stored_length() {
    let srv = start_server().await;
    let (id, job_id, epoch) = assign_and_start(&srv).await;
    let job_dir = srv._artifacts_dir.join(&id);
    std::fs::create_dir_all(&job_dir).unwrap();
    std::fs::write(job_dir.join("run.log"), b"0123456789").unwrap();
    // The stored length is shorter than the file; only the stored bytes are visible.
    srv.store.reserve_log(&job_id, epoch, 0, 4).unwrap();

    for (offset, expected) in [(0, "0123"), (2, "23")] {
        let r = raw_request(
            srv.addr,
            &format!("GET /v1/jobs/{id}/logs?offset={offset} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"),
        )
        .await;
        assert_eq!(status(&r), 200, "{r}");
        assert_eq!(body_str(&r), expected, "{r}");
        assert_eq!(header_value(&r, "x-lu-log-length"), Some("4"), "{r}");
    }
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn artifact_reads_refuse_files_over_the_stored_cap() {
    let srv = start_server().await;
    let id = JobId::new();
    let dir = srv._artifacts_dir.join(id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.json");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(MAX_REPORT_BYTES + 1)
        .unwrap();

    let r = raw_request(
        srv.addr,
        &operator_get(&format!("/v1/jobs/{id}/artifacts/report.json")),
    )
    .await;
    assert_eq!(status(&r), 413, "{r}");
    assert!(body_str(&r).contains("too_large"), "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn log_reads_refuse_files_over_the_stored_cap() {
    let srv = start_server().await;
    let (id, epoch) = assigned_epoch(&srv, &sample_spec(None)).await;
    srv.store
        .reserve_log(&JobId::parse(&id).unwrap(), epoch, 0, 1)
        .unwrap();
    let dir = srv._artifacts_dir.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::File::create(dir.join("run.log"))
        .unwrap()
        .set_len(MAX_LOG_BYTES + 1)
        .unwrap();

    let r = raw_request(srv.addr, &operator_get(&format!("/v1/jobs/{id}/logs"))).await;
    assert_eq!(status(&r), 413, "{r}");
    assert!(body_str(&r).contains("too_large"), "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
#[cfg(unix)]
async fn symlinked_artifact_and_log_reads_are_refused() {
    let srv = start_server().await;
    let (id, job_id, epoch) = assign_and_start(&srv).await;
    let dir = srv._artifacts_dir.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let secret = srv._artifacts_dir.join("secret.txt");
    std::fs::write(&secret, b"top secret\n").unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("report.json")).unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("run.log")).unwrap();
    srv.store.reserve_log(&job_id, epoch, 0, 11).unwrap();

    let r = raw_request(
        srv.addr,
        &operator_get(&format!("/v1/jobs/{id}/artifacts/report.json")),
    )
    .await;
    assert_eq!(status(&r), 404, "{r}");
    assert!(!body_str(&r).contains("top secret"), "{r}");

    let r = raw_request(srv.addr, &operator_get(&format!("/v1/jobs/{id}/logs"))).await;
    assert_eq!(status(&r), 404, "{r}");
    assert!(!body_str(&r).contains("top secret"), "{r}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
#[cfg(unix)]
async fn log_append_does_not_follow_a_symlink() {
    let srv = start_server().await;
    let (id, job_id, epoch) = assign_and_start(&srv).await;
    let dir = srv._artifacts_dir.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let secret = srv._artifacts_dir.join("secret.txt");
    std::fs::write(&secret, b"keep-me\n").unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("run.log")).unwrap();

    let chunk = b"overwritten\n";
    let req = format!(
        "POST /v1/agent/jobs/{id}/log?epoch={epoch}&offset=0 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n",
        chunk.len()
    );
    let mut packet = req.into_bytes();
    packet.extend_from_slice(chunk);
    let r = raw_request_bytes(srv.addr, &packet).await;
    assert_eq!(
        status(&String::from_utf8_lossy(&r)),
        500,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    assert_eq!(std::fs::read(&secret).unwrap(), b"keep-me\n");
    assert_eq!(srv.store.get_job(&job_id).unwrap().log_len, 0);
    let _ = srv.shutdown.send(());
}

fn repo_with_two_commits(root: &std::path::Path) -> (String, String) {
    let dir = root.join("source");
    std::fs::create_dir_all(&dir).unwrap();
    let repo = common::init_main_repo(&dir);
    let sig = Signature::now("tester", "tester@example.com").unwrap();
    let mut index = repo.index().unwrap();

    std::fs::write(dir.join("base.txt"), b"base\n").unwrap();
    index.add_path(std::path::Path::new("base.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let first = repo
        .commit(Some("refs/heads/main"), &sig, &sig, "first", &tree, &[])
        .unwrap();

    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("nested/child.txt"), b"child\n").unwrap();
    index
        .add_path(std::path::Path::new("nested/child.txt"))
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let second = repo
        .commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "second",
            &tree,
            &[&repo.find_commit(first).unwrap()],
        )
        .unwrap();
    (dir.to_string_lossy().into_owned(), second.to_string())
}

async fn base_request(addr: SocketAddr, id: &str, epoch: u64, token: &str) -> Vec<u8> {
    raw_request_bytes(
        addr,
        format!(
            "GET /v1/agent/jobs/{id}/base?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
}

fn split_http(raw: &[u8]) -> (&[u8], &[u8]) {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("complete HTTP response");
    (&raw[..end], &raw[end + 4..])
}

fn response_status(raw: &[u8]) -> u16 {
    let head = split_http(raw).0;
    let head = String::from_utf8_lossy(head);
    let line = head.lines().next().unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn response_header(raw: &[u8], name: &str) -> Option<String> {
    let head = split_http(raw).0;
    let text = String::from_utf8_lossy(head);
    let lower = name.to_ascii_lowercase();
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().to_ascii_lowercase() == lower).then(|| v.trim().to_owned())
    })
}

fn response_body(raw: &[u8]) -> &[u8] {
    split_http(raw).1
}

async fn assigned_epoch(srv: &TestServer, spec: &JobSpec) -> (String, u64) {
    let r = post_job(srv.addr, spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let j: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let id = j["id"].as_str().unwrap().to_owned();
    let poll_body = json!({"heartbeat": heartbeat_json("node-a"), "wait_secs": 0}).to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll_body.len(), poll_body
    );
    let r = raw_request(srv.addr, &req).await;
    assert_eq!(status(&r), 200, "{r}");
    let events: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "assign")
        .expect("job should be assigned");
    (id, assign["epoch"].as_u64().unwrap())
}

#[tokio::test]
async fn base_route_serves_a_pack_with_auth_node_and_epoch_fencing() {
    let root = tempfile::tempdir().unwrap();
    let (repo, commit) = repo_with_two_commits(root.path());
    let srv = start_server_with(vec![root.path().to_string_lossy().into_owned()], None).await;
    let mut spec = sample_spec(None);
    spec.repo = repo.clone();
    spec.source.repo = repo;
    spec.base_commit = Some(commit.clone());
    spec.source.base = Some(commit.clone());
    let (id, epoch) = assigned_epoch(&srv, &spec).await;

    let r = raw_request_bytes(
        srv.addr,
        format!("GET /v1/agent/jobs/{id}/base?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await;
    assert_eq!(response_status(&r), 401);

    let r = raw_request_bytes(
        srv.addr,
        format!("GET /v1/agent/jobs/{id}/base?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n").as_bytes(),
    )
    .await;
    assert_eq!(response_status(&r), 401);

    let r = base_request(srv.addr, &id, epoch, AGENT_B_TOKEN).await;
    assert_eq!(
        response_status(&r),
        403,
        "{:?}",
        String::from_utf8_lossy(&r)
    );

    let r = base_request(srv.addr, &id, epoch - 1, AGENT_A_TOKEN).await;
    assert_eq!(
        response_status(&r),
        409,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    let err: serde_json::Value =
        serde_json::from_slice(response_body(&r)).expect("stale epoch should return JSON");
    assert_eq!(err["error"], "stale_epoch", "{err}");

    let r = base_request(srv.addr, &id, epoch, AGENT_A_TOKEN).await;
    assert_eq!(
        response_status(&r),
        200,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    assert_eq!(
        response_header(&r, "content-type").as_deref(),
        Some("application/x-git-pack")
    );
    assert_eq!(
        response_header(&r, "x-lu-base-commit").as_deref(),
        Some(commit.as_str())
    );
    let body = response_body(&r);
    assert!(body.starts_with(b"PACK"), "pack header missing");
    assert!(body.len() > 12, "pack body is too short");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn base_packs_are_rate_limited_per_agent() {
    let root = tempfile::tempdir().unwrap();
    let (repo, commit) = repo_with_two_commits(root.path());
    let srv = start_server_with(vec![root.path().to_string_lossy().into_owned()], None).await;
    let mut spec = sample_spec(None);
    spec.repo = repo.clone();
    spec.source.repo = repo;
    spec.base_commit = Some(commit.clone());
    spec.source.base = Some(commit);
    let (id, epoch) = assigned_epoch(&srv, &spec).await;

    for _ in 0..PACK_PER_TOKEN {
        srv.state
            .pack_limiter
            .allow("node-a", PACK_PER_TOKEN)
            .unwrap();
    }
    let r = base_request(srv.addr, &id, epoch, AGENT_A_TOKEN).await;
    assert_eq!(
        response_status(&r),
        429,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    let err: serde_json::Value =
        serde_json::from_slice(response_body(&r)).expect("rate limit returns JSON");
    assert_eq!(err["error"], "rate_limited", "{err}");
    assert!(response_header(&r, "retry-after").is_some());

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn a_mirrored_repository_pins_at_submit_and_serves_its_base_from_the_mirror() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let (url, _first, second) = local_repo(repo_tmp.path());
    let srv = start_server_with_repos(vec![repo_entry(&url)]).await;

    let r = raw_request(srv.addr, &operator_post("/v1/repos/local/sync")).await;
    assert_eq!(status(&r), 200, "{r}");

    let mut spec = sample_spec(None);
    spec.repo = url.clone();
    spec.source.repo = url.clone();
    spec.git_ref = "main".to_owned();
    spec.source.git_ref = "main".to_owned();
    let (id, epoch) = assigned_epoch(&srv, &spec).await;

    let r = base_request(srv.addr, &id, epoch, AGENT_A_TOKEN).await;
    assert_eq!(
        response_status(&r),
        200,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    assert_eq!(
        response_header(&r, "x-lu-base-commit").as_deref(),
        Some(second.as_str())
    );
    assert!(
        response_body(&r).starts_with(b"PACK"),
        "pack header missing"
    );
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn base_route_is_404_without_a_provider_and_413_over_the_cap() {
    let srv = start_server().await;
    let mut spec = sample_spec(None);
    spec.base_commit = Some("a".repeat(40));
    spec.source.base = Some("a".repeat(40));
    let (id, epoch) = assigned_epoch(&srv, &spec).await;
    let r = base_request(srv.addr, &id, epoch, AGENT_A_TOKEN).await;
    assert_eq!(
        response_status(&r),
        404,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    let err: serde_json::Value =
        serde_json::from_slice(response_body(&r)).expect("missing provider should return JSON");
    assert_eq!(err["error"], "not_found", "{err}");
    let _ = srv.shutdown.send(());

    let root = tempfile::tempdir().unwrap();
    let (repo, commit) = repo_with_two_commits(root.path());
    let srv = start_server_with(vec![root.path().to_string_lossy().into_owned()], Some(1)).await;
    let mut spec = sample_spec(None);
    spec.repo = repo.clone();
    spec.source.repo = repo;
    spec.base_commit = Some(commit.clone());
    spec.source.base = Some(commit);
    let (id, epoch) = assigned_epoch(&srv, &spec).await;
    let r = base_request(srv.addr, &id, epoch, AGENT_A_TOKEN).await;
    assert_eq!(
        response_status(&r),
        413,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn configured_repositories_sync_list_and_pin_branch_tag_and_commit() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let (url, first, second) = local_repo(repo_tmp.path());
    let srv = start_server_with_repos(vec![repo_entry(&url)]).await;

    let r = raw_request(srv.addr, &operator_get("/v1/repos")).await;
    assert_eq!(status(&r), 200, "{r}");
    let listed: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(listed["repos"][0]["name"], "local");
    assert_eq!(listed["repos"][0]["url"], url);
    assert!(listed["repos"][0]["synced_at"].is_null(), "{listed}");
    assert_eq!(listed["repos"][0]["heads"], json!({}));

    let r = raw_request(srv.addr, &operator_post("/v1/repos/local/sync")).await;
    assert_eq!(status(&r), 200, "{r}");
    let synced: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(synced["name"], "local");
    assert_eq!(synced["heads"]["main"], second);
    assert!(synced["synced_at"].as_i64().unwrap() > 0, "{synced}");

    let mut spec = sample_spec(None);
    spec.repo = url.clone();
    spec.source.repo = url.clone();
    spec.git_ref = "main".to_owned();
    spec.source.git_ref = "main".to_owned();
    let r = post_job(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["spec"]["source"]["base"], second);

    spec.git_ref = "v1".to_owned();
    spec.source.git_ref = "v1".to_owned();
    let r = post_job(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["spec"]["source"]["base"], first);

    spec.git_ref = first.clone();
    spec.source.git_ref = first.clone();
    let r = post_job(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["spec"]["source"]["base"], first);

    spec.git_ref = "missing-branch".to_owned();
    spec.source.git_ref = "missing-branch".to_owned();
    let r = post_job(srv.addr, &spec).await;
    assert_eq!(status(&r), 422, "{r}");
    assert!(body_str(&r).contains("missing-branch"), "{r}");

    let r = post_job(srv.addr, &sample_spec(None)).await;
    assert_eq!(status(&r), 201, "{r}");
    let unlisted: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert!(unlisted["spec"].get("base_commit").is_none(), "{unlisted}");

    let r = raw_request(srv.addr, &operator_get("/v1/repos")).await;
    assert_eq!(status(&r), 200, "{r}");
    let listed: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert!(listed["repos"][0]["synced_at"].as_i64().unwrap() > 0);
    assert_eq!(listed["repos"][0]["heads"]["main"], second);

    let r = raw_request(
        srv.addr,
        &format!("POST /v1/repos/local/sync HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: 0\r\n\r\n"),
    )
    .await;
    assert_eq!(status(&r), 401, "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn a_pull_request_ref_is_pinned_at_submit_without_a_wildcard_sync() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let (url, first, _second) = local_repo(repo_tmp.path());
    let source = git2::Repository::open(repo_tmp.path()).unwrap();
    source
        .reference(
            "refs/pull/42/head",
            git2::Oid::from_str(&first).unwrap(),
            false,
            "pr head",
        )
        .unwrap();
    let srv = start_server_with_repos(vec![repo_entry(&url)]).await;

    let mut spec = sample_spec(None);
    spec.repo = url.clone();
    spec.source.repo = url;
    spec.git_ref = "refs/pull/42/head".to_owned();
    spec.source.git_ref = "refs/pull/42/head".to_owned();
    let r = post_job(srv.addr, &spec).await;
    assert_eq!(status(&r), 201, "{r}");
    let job: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(job["spec"]["source"]["base"], first);

    // A PR submission must not run the branch/tag sync that wildcard-fetches every head.
    let r = raw_request(srv.addr, &operator_get("/v1/repos")).await;
    assert_eq!(status(&r), 200, "{r}");
    let listed: serde_json::Value = serde_json::from_str(body_str(&r)).unwrap();
    assert_eq!(listed["repos"][0]["heads"], json!({}));

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn repository_sync_is_rate_limited_per_operator() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let (url, _first, _second) = local_repo(repo_tmp.path());
    let srv = start_server_with_repos(vec![repo_entry(&url)]).await;

    for _ in 0..SYNC_PER_TOKEN {
        srv.state.sync_limiter.allow("ops", SYNC_PER_TOKEN).unwrap();
    }
    assert!(srv.state.sync_limiter.allow("ops", SYNC_PER_TOKEN).is_err());
    let r = raw_request(srv.addr, &operator_post("/v1/repos/local/sync")).await;
    assert_eq!(status(&r), 429, "{r}");
    assert!(body_str(&r).contains("rate_limited"), "{r}");
    assert!(header_value(&r, "retry-after").is_some(), "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn repository_sync_is_refused_while_another_sync_holds_the_slot() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let (url, _first, _second) = local_repo(repo_tmp.path());
    let srv = start_server_with_repos(vec![repo_entry(&url)]).await;
    let _busy = Arc::clone(&srv.state.sync_slots)
        .try_acquire_owned()
        .unwrap();

    let r = raw_request(srv.addr, &operator_post("/v1/repos/local/sync")).await;
    assert_eq!(status(&r), 429, "{r}");
    assert!(body_str(&r).contains("rate_limited"), "{r}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn a_sync_error_never_contains_the_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let mut repo = repo_entry(&format!("file://{}/missing", tmp.path().display()));
    let credential = tmp.path().join("credential");
    std::fs::write(&credential, "user:super-secret-token\n").unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    repo.credential_file = Some(credential.display().to_string());
    let srv = start_server_with_repos(vec![repo]).await;

    let r = raw_request(srv.addr, &operator_post("/v1/repos/local/sync")).await;
    assert_eq!(status(&r), 500, "{r}");
    assert!(!r.contains("super-secret-token"), "{r}");
    assert!(!r.contains("user:super-secret-token"), "{r}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn artifacts_and_logs_are_scrubbed_before_storage() {
    let artifacts = ArtifactsConfig {
        scrub: true,
        extra_rules: vec![ScrubRuleConfig {
            name: "internal_ref".to_owned(),
            pattern: r"INTERNAL-[A-Z0-9]{6,}".to_owned(),
        }],
    };
    let srv = start_server_with_artifacts(Some(artifacts)).await;
    let (id, job_id, epoch) = assign_and_start(&srv).await;

    let patch = b"@@ -1,3 +1,3 @@\n context\n-token=abcdefghijklmnop\n+ghp_abcdefghijklmnopqrst\n INTERNAL-ABC12345\n";
    let req = format!(
        "PUT /v1/agent/jobs/{id}/artifacts/diff.patch?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n",
        patch.len()
    );
    let mut packet = req.into_bytes();
    packet.extend_from_slice(patch);
    let r = raw_request_bytes(srv.addr, &packet).await;
    assert_eq!(
        response_status(&r),
        200,
        "{:?}",
        String::from_utf8_lossy(&r)
    );

    let report = br#"{"password":"supersecretvalue"}"#;
    let req = format!(
        "PUT /v1/agent/jobs/{id}/artifacts/report.json?epoch={epoch} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n",
        report.len()
    );
    let mut packet = req.into_bytes();
    packet.extend_from_slice(report);
    let r = raw_request_bytes(srv.addr, &packet).await;
    assert_eq!(
        response_status(&r),
        200,
        "{:?}",
        String::from_utf8_lossy(&r)
    );

    let chunk = b"Authorization: Bearer abcdefghijklmnopqrst\n";
    let expected_log = b"Authorization: Bearer [scrubbed:bearer_token]\n";
    let req = format!(
        "POST /v1/agent/jobs/{id}/log?epoch={epoch}&offset=0 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Length: {}\r\n\r\n",
        chunk.len()
    );
    let mut packet = req.into_bytes();
    packet.extend_from_slice(chunk);
    let r = raw_request_bytes(srv.addr, &packet).await;
    assert_eq!(
        response_status(&r),
        200,
        "{:?}",
        String::from_utf8_lossy(&r)
    );
    let expected_len = expected_log.len().to_string();
    assert_eq!(
        response_header(&r, "x-lu-log-length").as_deref(),
        Some(expected_len.as_str())
    );

    let stored_patch = raw_request(
        srv.addr,
        &operator_get(&format!("/v1/jobs/{id}/artifacts/diff.patch")),
    )
    .await;
    let stored_patch = body_str(&stored_patch).to_owned();
    assert!(
        stored_patch.starts_with("@@ -1,3 +1,3 @@\n"),
        "{stored_patch}"
    );
    assert_eq!(
        stored_patch.lines().count(),
        patch.split(|b| *b == b'\n').count() - 1
    );
    assert!(
        stored_patch.contains("[scrubbed:assignment]"),
        "{stored_patch}"
    );
    assert!(
        stored_patch.contains("[scrubbed:token_prefix]"),
        "{stored_patch}"
    );
    assert!(
        stored_patch.contains("[scrubbed:internal_ref]"),
        "{stored_patch}"
    );
    assert!(!stored_patch.contains("abcdefghijklmnop"), "{stored_patch}");
    assert!(
        !stored_patch.contains("INTERNAL-ABC12345"),
        "{stored_patch}"
    );

    let stored_report = raw_request(
        srv.addr,
        &operator_get(&format!("/v1/jobs/{id}/artifacts/report.json")),
    )
    .await;
    assert!(
        body_str(&stored_report).contains("[scrubbed:assignment]"),
        "{stored_report}"
    );
    assert!(
        !body_str(&stored_report).contains("supersecretvalue"),
        "{stored_report}"
    );

    let stored_log = raw_request(srv.addr, &operator_get(&format!("/v1/jobs/{id}/logs"))).await;
    assert_eq!(body_str(&stored_log), String::from_utf8_lossy(expected_log));

    let view = raw_request(srv.addr, &operator_get(&format!("/v1/jobs/{id}"))).await;
    let view: serde_json::Value = serde_json::from_str(body_str(&view)).unwrap();
    assert_eq!(view["scrubbed_hits"], 5, "{view}");
    assert_eq!(view["patch_scrubbed"], true, "{view}");

    let rec = srv.store.get_job(&job_id).unwrap();
    assert_eq!(rec.scrubbed_hits, 5);
    assert!(rec.patch_scrubbed);
    let _ = srv.shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_can_set_a_runtime_group_limit() {
    let srv = start_server().await;
    let put = |path: &str, body: &str| {
        format!(
            "PUT {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        )
    };

    let ok = raw_request(
        srv.addr,
        &put("/v1/groups/batch/limit", r#"{"max_running":2}"#),
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    let view: serde_json::Value = serde_json::from_str(body_str(&ok)).unwrap();
    assert_eq!(view, json!({"group": "batch", "max_running": 2}));
    assert_eq!(srv.state.group_limits().get("batch"), Some(&2));

    let unknown = raw_request(
        srv.addr,
        &put("/v1/groups/batch/limit", r#"{"max_running":2,"oops":1}"#),
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 422"), "{unknown}");

    let range = raw_request(
        srv.addr,
        &put("/v1/groups/batch/limit", r#"{"max_running":0}"#),
    )
    .await;
    assert!(range.starts_with("HTTP/1.1 400"), "{range}");
    assert!(body_str(&range).contains("max_running must be 1 to 64"));

    let group = raw_request(
        srv.addr,
        &put("/v1/groups/bad%20name/limit", r#"{"max_running":2}"#),
    )
    .await;
    assert!(group.starts_with("HTTP/1.1 400"), "{group}");
    assert_eq!(srv.state.group_limits().get("batch"), Some(&2));
    let _ = srv.shutdown.send(());
}
