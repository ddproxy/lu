#[path = "support/fake_store.rs"]
mod fake_store;

use fake_store::FakeStore;
use lu_auth::sha256_hex;
use lu_config::{ControllerAgent, ControllerConfig, ControllerOperator};
use lu_controller::{build_router, run_background, AppState};
use lu_proto::{JobId, JobSpec, MAX_AFTER_JOBS};
use lu_serve::{serve, ServeOptions};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

const OP_TOKEN: &str = "op-token-1234567890";
const AGENT_A_TOKEN: &str = "agent-a-token-1234567890";
const AGENT_B_TOKEN: &str = "agent-b-token-1234567890";

fn controller_cfg(artifacts_dir: &str) -> ControllerConfig {
    ControllerConfig {
        listen: "127.0.0.1:0".to_owned(),
        db: "/tmp/lu-test.db".to_owned(),
        artifacts_dir: artifacts_dir.to_owned(),
        lease_secs: 6,
        max_retries: 1,
        provider_cooldown_secs: 60,
        recovery_grace_secs: 1,
        max_connections: 64,
        max_connections_per_addr: 32,
        agents: vec![
            ControllerAgent {
                name: "node-a".to_owned(),
                token_sha256: sha256_hex(AGENT_A_TOKEN),
                cordoned: false,
            },
            ControllerAgent {
                name: "node-b".to_owned(),
                token_sha256: sha256_hex(AGENT_B_TOKEN),
                cordoned: false,
            },
        ],
        operators: vec![ControllerOperator {
            name: "ops".to_owned(),
            token_sha256: sha256_hex(OP_TOKEN),
        }],
    }
}

struct TestServer {
    addr: SocketAddr,
    shutdown: broadcast::Sender<()>,
    _handle: tokio::task::JoinHandle<()>,
    _bg: tokio::task::JoinHandle<()>,
    _artifacts_dir: std::path::PathBuf,
}

async fn start_server() -> TestServer {
    let dir =
        std::env::temp_dir().join(format!("lu-ctl-{}-{}", std::process::id(), fastrand_u64()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = controller_cfg(dir.to_str().unwrap());
    let store = Arc::new(FakeStore::new());
    let state = Arc::new(AppState::from_config(&cfg, store).unwrap());
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
        build_router(state),
        options,
        None,
        async move {
            let _ = rx1.recv().await;
        },
    ));
    TestServer {
        addr,
        shutdown: tx,
        _handle: handle,
        _bg: bg,
        _artifacts_dir: dir,
    }
}

fn fastrand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
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
        repo: "https://example.net/repo.git".to_owned(),
        git_ref: "main".to_owned(),
        task: "run tests".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 3600,
        idle_timeout_secs: None,
        idempotency_key: key.map(|s| s.to_owned()),
        after: Vec::new(),
    }
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

fn heartbeat_json(node: &str) -> serde_json::Value {
    json!({"node": node, "harness": "idle", "providers": []})
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

#[tokio::test]
async fn node_stats_are_served_bounded_to_operator_and_clear_on_next_heartbeat() {
    let srv = start_server().await;
    let stats_url = |node: &str| {
        format!("GET /v1/nodes/{node}/stats HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n")
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

    // a known node with no reported stats serializes as an empty object
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
    assert_eq!(empty, json!({}), "{r}");

    let job_id = JobId::new().to_string();
    let heartbeat = json!({
        "node": "node-a",
        "harness": "busy",
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
    assert_eq!(cleared, json!({}), "{r}");

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
        "repo":"https://example.net/repo.git",
        "git_ref":"main",
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
