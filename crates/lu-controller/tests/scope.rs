//! Integration tests for `/v1/scope/...`: the controller-side security boundary for jobs that
//! submit follow-up work. These drive the real router, auth, store, audit writer and metrics.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lu_auth::sha256_hex;
use lu_config::{
    ArtifactsConfig, ControllerAgent, ControllerConfig, ControllerOperator, ScopeConfig,
};
use lu_controller::{build_router, recover_on_boot, run_background, AppState};
use lu_proto::{ArtifactName, ScopeAction, ScopePayload, ScopeRequest, SCOPE_ROUTES, SCOPE_SOURCE};
use lu_serve::{serve, ServeOptions};
use lu_store::sqlite::SqliteStore;
use lu_store::{JobRecord, Store};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

const OP_TOKEN: &str = "scope-op-token-1234567890";
const AGENT_A_TOKEN: &str = "scope-agent-a-token-1234567890";
const AGENT_B_TOKEN: &str = "scope-agent-b-token-1234567890";
const AGENT_C_TOKEN: &str = "scope-agent-c-token-1234567890";

fn scope_config() -> ScopeConfig {
    ScopeConfig {
        enabled: true,
        max_children_per_job: 8,
        max_descendants_per_root: 32,
        max_depth: 2,
        max_task_bytes: 1024,
        max_spawn_per_minute: 6,
        max_artifact_read_bytes: 4096,
        max_summary_bytes: 4096,
    }
}

fn controller_cfg(
    db: &str,
    artifacts: &str,
    lease_secs: u64,
    recovery_grace_secs: u64,
) -> ControllerConfig {
    ControllerConfig {
        listen: "127.0.0.1:0".to_owned(),
        db: db.to_owned(),
        artifacts_dir: artifacts.to_owned(),
        lease_secs,
        max_retries: 1,
        provider_cooldown_secs: 60,
        recovery_grace_secs,
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
            ControllerAgent {
                name: "node-c".to_owned(),
                token_sha256: sha256_hex(AGENT_C_TOKEN),
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
    state: Arc<AppState>,
    store: Arc<SqliteStore>,
    audit_path: Option<PathBuf>,
    artifact_dir: PathBuf,
    shutdown: broadcast::Sender<()>,
    _handle: tokio::task::JoinHandle<()>,
    _bg: tokio::task::JoinHandle<()>,
    _audit_task: Option<tokio::task::JoinHandle<()>>,
    _tmp: tempfile::TempDir,
}

async fn start_server(scope: Option<ScopeConfig>) -> TestServer {
    start_server_with(scope, false, 6, 60).await
}

async fn start_server_with(
    scope: Option<ScopeConfig>,
    audit: bool,
    lease_secs: u64,
    recovery_grace_secs: u64,
) -> TestServer {
    start_server_with_artifacts(scope, audit, lease_secs, recovery_grace_secs, None).await
}

async fn start_server_with_artifacts(
    scope: Option<ScopeConfig>,
    audit: bool,
    lease_secs: u64,
    recovery_grace_secs: u64,
    artifacts: Option<&ArtifactsConfig>,
) -> TestServer {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("lu.db");
    let artifact_dir = tmp.path().join("artifacts");
    let audit_path = audit.then(|| tmp.path().join("audit.jsonl"));
    let cfg = controller_cfg(
        db.to_str().unwrap(),
        artifact_dir.to_str().unwrap(),
        lease_secs,
        recovery_grace_secs,
    );
    let store = Arc::new(SqliteStore::open(&db).unwrap());
    let state =
        AppState::from_config_with_scope(&cfg, store.clone(), artifacts, scope.as_ref()).unwrap();
    let audit_task = if let Some(path) = &audit_path {
        let (writer, task) = lu_audit::Writer::open(path, Arc::clone(&state.audit_write_errors))
            .await
            .unwrap();
        state.set_audit(writer);
        Some(task)
    } else {
        None
    };
    let state = Arc::new(state);
    recover_on_boot(&state).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown, _) = broadcast::channel::<()>(4);
    let mut rx1 = shutdown.subscribe();
    let mut rx2 = shutdown.subscribe();
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
        state,
        store,
        audit_path,
        artifact_dir,
        shutdown,
        _handle: handle,
        _bg: bg,
        _audit_task: audit_task,
        _tmp: tmp,
    }
}

async fn raw_request(addr: SocketAddr, req: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
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
    resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

fn json_body(resp: &str) -> Value {
    serde_json::from_str(body(resp))
        .unwrap_or_else(|error| panic!("bad body {}: {error}", body(resp)))
}

fn error_code(resp: &str) -> String {
    let value = json_body(resp);
    value["error"]["code"]
        .as_str()
        .or_else(|| value["error"].as_str())
        .unwrap_or("<missing>")
        .to_owned()
}

fn error_limit(resp: &str) -> Option<String> {
    json_body(resp)["error"]["limit"]
        .as_str()
        .map(str::to_owned)
}

fn sample_spec() -> lu_proto::JobSpec {
    lu_proto::JobSpec {
        source: lu_proto::SourceSpec {
            repo: "https://example.net/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            base: None,
        },
        repo: "https://example.net/repo.git".to_owned(),
        git_ref: "main".to_owned(),
        base_commit: None,
        task: "operator task".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 3600,
        idle_timeout_secs: None,
        on_stuck: None,
        stuck_after_secs: None,
        idempotency_key: None,
        after: Vec::new(),
        collect: lu_proto::CollectMode::None,
        include_failed: false,
        on_conflict: lu_proto::OnConflict::Fail,
        priority: lu_proto::DEFAULT_PRIORITY,
        group: None,
        allow_spawn: true,
        spawn_inherit: true,
        spawn_repos: Vec::new(),
        workspace: None,
        cache_dirs: Vec::new(),
        context: None,
        origin: None,
        origin_ref: None,
        untrusted_context: false,
        draft: false,
        publish: None,
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        fallbacks: Vec::new(),
        expect: None,
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
    }
}

async fn submit_spec(addr: SocketAddr, spec: &lu_proto::JobSpec) -> String {
    let bytes = serde_json::to_vec(spec).unwrap();
    let req = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        bytes.len(),
        String::from_utf8(bytes).unwrap()
    );
    raw_request(addr, &req).await
}

async fn poll_assign_as(
    srv: &TestServer,
    token: &str,
    node: &str,
    wanted: Option<&str>,
    providers: &[&str],
) -> u64 {
    let names: Vec<&str> = providers.to_vec();
    let provider_status: Vec<Value> = names
        .iter()
        .map(|name| json!({"name": name, "state": "ok"}))
        .collect();
    let models: Vec<Value> = names
        .iter()
        .map(|name| json!({"provider": name, "model": "m1"}))
        .collect();
    let poll = json!({
        "heartbeat": {
            "node": node,
            "harness": "idle",
            "providers": provider_status,
            "provider_models": models,
            "labels": ["a", "b", "c"]
        },
        "wait_secs": 0
    })
    .to_string();
    let req = format!(
        "POST /v1/agent/poll HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        poll.len(),
        poll
    );
    let resp = raw_request(srv.addr, &req).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let events = json_body(&resp);
    let assign = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "assign" && wanted.is_none_or(|id| event["job_id"] == id))
        .unwrap_or_else(|| panic!("no assignment for {wanted:?}: {events}"));
    assign["epoch"].as_u64().unwrap()
}

async fn start_job_as(srv: &TestServer, token: &str, id: &str, epoch: u64) {
    let payload = json!({"epoch": epoch}).to_string();
    let req = format!(
        "POST /v1/agent/jobs/{id}/start HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        payload.len(),
        payload
    );
    let resp = raw_request(srv.addr, &req).await;
    assert_eq!(status(&resp), 200, "{resp}");
}

async fn lease(srv: &TestServer, id: &str, providers: &[&str]) -> (lu_proto::JobId, u64) {
    lease_as(srv, AGENT_A_TOKEN, "node-a", id, providers).await
}

async fn lease_as(
    srv: &TestServer,
    token: &str,
    node: &str,
    id: &str,
    providers: &[&str],
) -> (lu_proto::JobId, u64) {
    let epoch = poll_assign_as(srv, token, node, Some(id), providers).await;
    start_job_as(srv, token, id, epoch).await;
    (lu_proto::JobId::parse(id).unwrap(), epoch)
}

async fn lease_spec(srv: &TestServer, spec: &lu_proto::JobSpec) -> (String, lu_proto::JobId, u64) {
    lease_spec_with_providers(srv, spec, &[]).await
}

async fn lease_spec_with_providers(
    srv: &TestServer,
    spec: &lu_proto::JobSpec,
    providers: &[&str],
) -> (String, lu_proto::JobId, u64) {
    lease_spec_as(srv, AGENT_A_TOKEN, "node-a", spec, providers).await
}

async fn lease_spec_as(
    srv: &TestServer,
    token: &str,
    node: &str,
    spec: &lu_proto::JobSpec,
    providers: &[&str],
) -> (String, lu_proto::JobId, u64) {
    let resp = submit_spec(srv.addr, spec).await;
    assert_eq!(status(&resp), 201, "{resp}");
    let id = json_body(&resp)["id"].as_str().unwrap().to_owned();
    let (job_id, epoch) = lease_as(srv, token, node, &id, providers).await;
    (id, job_id, epoch)
}

async fn finish(srv: &TestServer, id: &str, providers: &[&str]) {
    let (_, epoch) = lease(srv, id, providers).await;
    let payload =
        json!({"job_id": id, "epoch": epoch, "outcome": "done", "exit_code": 0}).to_string();
    let req = format!(
        "POST /v1/agent/jobs/{id}/complete HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        payload.len(),
        payload
    );
    let resp = raw_request(srv.addr, &req).await;
    assert_eq!(status(&resp), 200, "{resp}");
}

fn request_for(action: ScopeAction, id: &lu_proto::JobId, epoch: u64) -> ScopeRequest {
    let payload = match action {
        ScopeAction::SubmitChild => Some(ScopePayload {
            task: Some("child task".to_owned()),
            ..ScopePayload::default()
        }),
        ScopeAction::GetChild | ScopeAction::CancelChild | ScopeAction::GetChildArtifact => {
            let mut payload = ScopePayload {
                child: Some(lu_proto::JobId::new()),
                ..ScopePayload::default()
            };
            if action == ScopeAction::GetChildArtifact {
                payload.artifact = Some("report.json".to_owned());
            }
            Some(payload)
        }
        ScopeAction::JobContext | ScopeAction::ListChildren => None,
    };
    let mut request = ScopeRequest::new(id.clone(), epoch, action);
    if let Some(payload) = payload {
        request = request.with_payload(payload);
    }
    request
}

async fn call_scope(
    srv: &TestServer,
    route: &str,
    token: Option<&str>,
    request: &ScopeRequest,
) -> String {
    let bytes = serde_json::to_vec(request).unwrap();
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST /v1/scope/{route} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        bytes.len(),
        String::from_utf8(bytes).unwrap()
    );
    raw_request(srv.addr, &req).await
}

async fn scope_ok(
    srv: &TestServer,
    action: ScopeAction,
    id: &lu_proto::JobId,
    epoch: u64,
    payload: Option<ScopePayload>,
) -> Value {
    scope_ok_as(srv, AGENT_A_TOKEN, action, id, epoch, payload).await
}

async fn scope_ok_as(
    srv: &TestServer,
    token: &str,
    action: ScopeAction,
    id: &lu_proto::JobId,
    epoch: u64,
    payload: Option<ScopePayload>,
) -> Value {
    let mut request = ScopeRequest::new(id.clone(), epoch, action);
    if let Some(payload) = payload {
        request = request.with_payload(payload);
    }
    let resp = call_scope(srv, action.as_str(), Some(token), &request).await;
    let code = status(&resp);
    assert!(
        code == 200 || (code == 201 && action == ScopeAction::SubmitChild),
        "{resp}"
    );
    assert_eq!(json_body(&resp)["source"], SCOPE_SOURCE);
    assert_eq!(json_body(&resp)["untrusted"], true);
    json_body(&resp)["result"].clone()
}

fn read_tree(path: &Path) -> String {
    let mut out = String::new();
    if path.is_file() {
        if let Ok(bytes) = std::fs::read(path) {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
        return out;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return out;
    };
    for entry in entries.flatten() {
        out.push_str(&read_tree(&entry.path()));
    }
    out
}

async fn artifact_setup(srv: &TestServer) -> (lu_proto::JobId, lu_proto::JobId, u64, u64) {
    let (_, parent, epoch) = lease_spec(srv, &sample_spec()).await;
    let child = scope_ok(
        srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("child".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let child_id = lu_proto::JobId::parse(child["job_id"].as_str().unwrap()).unwrap();
    let (_, child_epoch) = lease_as(srv, AGENT_B_TOKEN, "node-b", child_id.as_str(), &["p1"]).await;
    (parent, child_id, epoch, child_epoch)
}

fn assert_limit(resp: &str, expected: &str) {
    assert_eq!(status(resp), 429, "{resp}");
    assert_eq!(error_code(resp), "limit_exceeded", "{resp}");
    assert_eq!(error_limit(resp).as_deref(), Some(expected), "{resp}");
    assert_eq!(json_body(resp)["source"], SCOPE_SOURCE);
    assert_eq!(json_body(resp)["untrusted"], true);
    assert!(
        !body(resp).contains("operator task"),
        "task text leaked: {resp}"
    );
}

#[tokio::test]
async fn only_an_agent_leased_to_the_job_can_use_scope_routes() {
    let srv = start_server(Some(scope_config())).await;
    let (id, job_id, epoch) = lease_spec(&srv, &sample_spec()).await;

    for (index, route) in SCOPE_ROUTES.iter().enumerate() {
        let action = ScopeAction::parse(route).unwrap();
        let request = request_for(action, &job_id, epoch);

        // Operator credentials are never accepted on a scope route.
        let resp = call_scope(&srv, route, Some(OP_TOKEN), &request).await;
        assert_eq!(status(&resp), 401, "{route}: {resp}");
        assert_eq!(error_code(&resp), "unauthorized");

        if index == 0 {
            // Neither is an absent credential.
            let resp = call_scope(&srv, route, None, &request).await;
            assert_eq!(status(&resp), 401, "{route}: {resp}");

            // A valid agent token for a different node cannot act on this job.
            let resp = call_scope(&srv, route, Some(AGENT_B_TOKEN), &request).await;
            assert_eq!(status(&resp), 409, "{route}: {resp}");
            assert_eq!(error_code(&resp), "stale_epoch", "{route}: {resp}");
        }

        // The leasing agent may call the route. Child-specific routes use a made-up id, so a
        // 404 is the expected result there; it still proves the agent is authenticated.
        let resp = call_scope(&srv, route, Some(AGENT_A_TOKEN), &request).await;
        let code = status(&resp);
        let child_route = matches!(
            action,
            ScopeAction::GetChild | ScopeAction::CancelChild | ScopeAction::GetChildArtifact
        );
        assert!(
            code == 200
                || (code == 201 && action == ScopeAction::SubmitChild)
                || (code == 404 && child_route),
            "{route}: {resp}"
        );
    }

    let unknown = call_scope(
        &srv,
        "drain",
        Some(AGENT_A_TOKEN),
        &request_for(ScopeAction::JobContext, &job_id, epoch),
    )
    .await;
    assert_eq!(status(&unknown), 404, "{unknown}");
    assert_eq!(error_code(&unknown), "not_found");
    let _ = id;
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn job_context_and_unrelated_ids_do_not_leak_other_jobs() {
    let srv = start_server(Some(scope_config())).await;

    // A dependency is finished so the parent can be leased independently.
    let mut dep = sample_spec();
    dep.task = "DEPENDENCY_SECRET_TASK".to_owned();
    dep.idempotency_key = Some("dep-key".to_owned());
    let dep_resp = submit_spec(srv.addr, &dep).await;
    let dep_id = json_body(&dep_resp)["id"].as_str().unwrap().to_owned();
    finish(&srv, &dep_id, &[]).await;

    let mut spec = sample_spec();
    spec.task = "PARENT_SECRET_TASK".to_owned();
    spec.after = vec![lu_proto::JobId::parse(&dep_id).unwrap()];
    let (id, job_id, epoch) = lease_spec(&srv, &spec).await;

    let context = scope_ok(&srv, ScopeAction::JobContext, &job_id, epoch, None).await;
    assert_eq!(context["kind"], "job_context");
    assert_eq!(context["job_id"], id);
    assert_eq!(context["spec"]["task"], "PARENT_SECRET_TASK");
    let deps = context["dependencies"].as_array().unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0]["job_id"], dep_id);
    assert_eq!(deps[0]["idempotency_key"], "dep-key");
    assert!(!context.to_string().contains("DEPENDENCY_SECRET_TASK"));

    let other_resp = submit_spec(srv.addr, &sample_spec()).await;
    let other_id = json_body(&other_resp)["id"].as_str().unwrap().to_owned();
    let missing = lu_proto::JobId::new().to_string();

    for target in [&other_id, &missing, &id] {
        let request = ScopeRequest::new(job_id.clone(), epoch, ScopeAction::GetChild).with_payload(
            ScopePayload {
                child: Some(lu_proto::JobId::parse(target).unwrap()),
                ..ScopePayload::default()
            },
        );
        let resp = call_scope(&srv, "get_child", Some(AGENT_A_TOKEN), &request).await;
        assert_eq!(status(&resp), 404, "{target}: {resp}");
        assert_eq!(error_code(&resp), "not_found");
        assert_eq!(body(&resp), "{\"source\":\"child_job\",\"untrusted\":true,\"error\":{\"code\":\"not_found\",\"message\":\"not found\"}}", "{target}");
        assert!(!body(&resp).contains("SECRET"));
    }

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn submit_child_clamps_policy_and_namespaces_idempotency() {
    let srv = start_server(Some(scope_config())).await;
    let mut spec = sample_spec();
    spec.provider = Some("p1".to_owned());
    spec.model = Some("m1".to_owned());
    spec.fallbacks = vec![lu_proto::Choice {
        provider: "p2".to_owned(),
        model: Some("m2".to_owned()),
    }];
    spec.priority = 7;
    spec.timeout_secs = 100;
    spec.requires = vec!["a".to_owned(), "b".to_owned()];
    spec.idempotency_key = Some("parent-key".to_owned());
    let (parent_id, parent, epoch) = lease_spec_with_providers(&srv, &spec, &["p1"]).await;

    let child = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("child task".to_owned()),
            repo: Some(spec.repo.clone()),
            provider: Some("p2".to_owned()),
            model: Some("m2".to_owned()),
            priority: Some(255),
            timeout_secs: Some(86_000),
            requires: Some(vec!["a".to_owned()]),
            idempotency_key: Some("k1".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let child_id = lu_proto::JobId::parse(child["job_id"].as_str().unwrap()).unwrap();
    let record = srv.store.get_job(&child_id).unwrap();
    assert_eq!(record.parent_job_id.as_ref(), Some(&parent));
    assert_eq!(record.root_job_id.as_ref(), Some(&parent));
    assert_eq!(record.depth, 1);
    assert_eq!(record.spec.repo, spec.repo);
    assert_eq!(record.spec.git_ref, spec.git_ref);
    assert_eq!(record.spec.base_commit, None);
    assert_eq!(record.spec.collect, lu_proto::CollectMode::None);
    assert_eq!(record.spec.expect, None);
    assert_eq!(record.spec.group, None);
    assert_eq!(record.spec.provider.as_deref(), Some("p2"));
    assert_eq!(record.spec.model.as_deref(), Some("m2"));
    assert_eq!(record.spec.priority, 7);
    assert!(record.spec.timeout_secs <= 100);
    assert_eq!(record.spec.requires, vec!["a".to_owned()]);
    assert_eq!(
        record.spec.idempotency_key.as_deref(),
        Some(format!("scope:{parent_id}:k1").as_str())
    );
    assert!(srv.store.job_by_key("k1").is_none());

    // The same scoped key is idempotent for the same parent.
    let again = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("child task".to_owned()),
            provider: Some("p2".to_owned()),
            model: Some("m2".to_owned()),
            priority: Some(255),
            timeout_secs: Some(86_000),
            requires: Some(vec!["a".to_owned()]),
            idempotency_key: Some("k1".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    assert_eq!(again["job_id"], child_id.to_string());

    // The raw key remains available to operators, so the namespaces cannot collide or probe.
    let mut raw = sample_spec();
    raw.idempotency_key = Some("k1".to_owned());
    let raw_resp = submit_spec(srv.addr, &raw).await;
    assert_eq!(status(&raw_resp), 201, "{raw_resp}");
    assert!(srv.store.job_by_key("k1").is_some());

    let refusals = [
        (
            ScopePayload {
                task: Some("x".to_owned()),
                repo: Some("https://elsewhere.example/r.git".to_owned()),
                ..ScopePayload::default()
            },
            400,
        ),
        (
            ScopePayload {
                task: Some("x".to_owned()),
                provider: Some("p3".to_owned()),
                ..ScopePayload::default()
            },
            400,
        ),
        (
            ScopePayload {
                task: Some("x".to_owned()),
                requires: Some(vec!["c".to_owned()]),
                ..ScopePayload::default()
            },
            400,
        ),
        (
            ScopePayload {
                task: Some("x".to_owned()),
                after: Some(vec![lu_proto::JobId::new()]),
                ..ScopePayload::default()
            },
            404,
        ),
        (
            ScopePayload {
                task: Some("x".repeat(1025)),
                ..ScopePayload::default()
            },
            429,
        ),
    ];
    for (payload, expected) in refusals {
        let request = ScopeRequest::new(parent.clone(), epoch, ScopeAction::SubmitChild)
            .with_payload(payload);
        let resp = call_scope(&srv, "submit_child", Some(AGENT_A_TOKEN), &request).await;
        assert_eq!(status(&resp), expected, "{resp}");
        if expected == 429 {
            assert_limit(&resp, "max_task_bytes");
        }
    }

    // A job without the operator-granted capability cannot spawn even with a leased socket.
    let mut no_spawn = sample_spec();
    no_spawn.allow_spawn = false;
    no_spawn.priority = 9;
    let (_, plain, plain_epoch) =
        lease_spec_as(&srv, AGENT_C_TOKEN, "node-c", &no_spawn, &[]).await;
    let request = ScopeRequest::new(plain, plain_epoch, ScopeAction::SubmitChild).with_payload(
        ScopePayload {
            task: Some("x".to_owned()),
            ..ScopePayload::default()
        },
    );
    let resp = call_scope(&srv, "submit_child", Some(AGENT_C_TOKEN), &request).await;
    assert_eq!(status(&resp), 403, "{resp}");

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn every_scope_limit_has_a_typed_error() {
    // Disabled by default, even when the caller is a valid leased agent.
    let srv = start_server(None).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let request = ScopeRequest::new(parent, epoch, ScopeAction::JobContext);
    let resp = call_scope(&srv, "job_context", Some(AGENT_A_TOKEN), &request).await;
    assert_limit(&resp, "scope_enabled");
    let _ = srv.shutdown.send(());

    // Children per job.
    let mut cfg = scope_config();
    cfg.max_children_per_job = 1;
    let srv = start_server(Some(cfg)).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let payload = ScopePayload {
        task: Some("child".to_owned()),
        ..ScopePayload::default()
    };
    assert_eq!(
        status(
            &call_scope(
                &srv,
                "submit_child",
                Some(AGENT_A_TOKEN),
                &ScopeRequest::new(parent.clone(), epoch, ScopeAction::SubmitChild)
                    .with_payload(payload.clone())
            )
            .await
        ),
        201
    );
    let resp = call_scope(
        &srv,
        "submit_child",
        Some(AGENT_A_TOKEN),
        &ScopeRequest::new(parent, epoch, ScopeAction::SubmitChild).with_payload(payload),
    )
    .await;
    assert_limit(&resp, "max_children_per_job");
    let _ = srv.shutdown.send(());

    // Spawns per minute.
    let mut cfg = scope_config();
    cfg.max_spawn_per_minute = 1;
    let srv = start_server(Some(cfg)).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let payload = ScopePayload {
        task: Some("child".to_owned()),
        ..ScopePayload::default()
    };
    assert_eq!(
        status(
            &call_scope(
                &srv,
                "submit_child",
                Some(AGENT_A_TOKEN),
                &ScopeRequest::new(parent.clone(), epoch, ScopeAction::SubmitChild)
                    .with_payload(payload.clone())
            )
            .await
        ),
        201
    );
    let resp = call_scope(
        &srv,
        "submit_child",
        Some(AGENT_A_TOKEN),
        &ScopeRequest::new(parent, epoch, ScopeAction::SubmitChild).with_payload(payload),
    )
    .await;
    assert_limit(&resp, "max_spawn_per_minute");
    let _ = srv.shutdown.send(());

    // Depth.
    let mut cfg = scope_config();
    cfg.max_depth = 1;
    let srv = start_server(Some(cfg)).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let child = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("child".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let child_id = lu_proto::JobId::parse(child["job_id"].as_str().unwrap()).unwrap();
    let (_, child_epoch) =
        lease_as(&srv, AGENT_B_TOKEN, "node-b", child_id.as_str(), &["p1"]).await;
    let resp = call_scope(
        &srv,
        "submit_child",
        Some(AGENT_B_TOKEN),
        &ScopeRequest::new(child_id, child_epoch, ScopeAction::SubmitChild).with_payload(
            ScopePayload {
                task: Some("grandchild".to_owned()),
                ..ScopePayload::default()
            },
        ),
    )
    .await;
    assert_limit(&resp, "max_depth");
    let _ = srv.shutdown.send(());

    // Descendants of a root.
    let mut cfg = scope_config();
    cfg.max_descendants_per_root = 1;
    let srv = start_server(Some(cfg)).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let child = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("child".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let child_id = lu_proto::JobId::parse(child["job_id"].as_str().unwrap()).unwrap();
    let (_, child_epoch) =
        lease_as(&srv, AGENT_B_TOKEN, "node-b", child_id.as_str(), &["p1"]).await;
    let resp = call_scope(
        &srv,
        "submit_child",
        Some(AGENT_B_TOKEN),
        &ScopeRequest::new(child_id, child_epoch, ScopeAction::SubmitChild).with_payload(
            ScopePayload {
                task: Some("grandchild".to_owned()),
                ..ScopePayload::default()
            },
        ),
    )
    .await;
    assert_limit(&resp, "max_descendants_per_root");
    let _ = srv.shutdown.send(());

    // Artifact reads.
    let srv = start_server(Some(scope_config())).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let child = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("child".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let child_id = lu_proto::JobId::parse(child["job_id"].as_str().unwrap()).unwrap();
    let (_, child_epoch) =
        lease_as(&srv, AGENT_B_TOKEN, "node-b", child_id.as_str(), &["p1"]).await;
    srv.store
        .record_artifact(&child_id, child_epoch, ArtifactName::ReportJson, 5000)
        .unwrap();
    let artifact_dir = srv.artifact_dir.join(child_id.as_str());
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::write(artifact_dir.join("report.json"), vec![b'x'; 5000]).unwrap();
    std::fs::write(artifact_dir.join("run.log"), b"secret log").unwrap();

    let request = ScopeRequest::new(parent.clone(), epoch, ScopeAction::GetChildArtifact)
        .with_payload(ScopePayload {
            child: Some(child_id.clone()),
            artifact: Some("report.json".to_owned()),
            ..ScopePayload::default()
        });
    let resp = call_scope(&srv, "get_child_artifact", Some(AGENT_A_TOKEN), &request).await;
    assert_limit(&resp, "max_artifact_read_bytes");

    let request = ScopeRequest::new(parent, epoch, ScopeAction::GetChildArtifact).with_payload(
        ScopePayload {
            child: Some(child_id),
            artifact: Some("run.log".to_owned()),
            ..ScopePayload::default()
        },
    );
    let resp = call_scope(&srv, "get_child_artifact", Some(AGENT_A_TOKEN), &request).await;
    assert_eq!(status(&resp), 400, "{resp}");
    assert!(!body(&resp).contains("secret log"));

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn maximum_scope_request_size_is_accepted_end_to_end() {
    let mut cfg = scope_config();
    cfg.max_task_bytes = 256 * 1024;
    let srv = start_server(Some(cfg)).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;

    let task = "x".repeat(256 * 1024);
    let response = call_scope(
        &srv,
        "submit_child",
        Some(AGENT_A_TOKEN),
        &ScopeRequest::new(parent.clone(), epoch, ScopeAction::SubmitChild).with_payload(
            ScopePayload {
                task: Some(task.clone()),
                ..ScopePayload::default()
            },
        ),
    )
    .await;
    assert_eq!(status(&response), 201, "{response}");

    // One byte over the proto-wide cap is rejected by the shared decoder before the controller
    // clamp sees it.
    let too_large = call_scope(
        &srv,
        "submit_child",
        Some(AGENT_A_TOKEN),
        &ScopeRequest::new(parent, epoch, ScopeAction::SubmitChild).with_payload(ScopePayload {
            task: Some(format!("{task}x")),
            ..ScopePayload::default()
        }),
    )
    .await;
    assert_eq!(status(&too_large), 400, "{too_large}");
    assert_eq!(error_code(&too_large), "bad_request", "{too_large}");
    assert_eq!(json_body(&too_large)["source"], SCOPE_SOURCE);

    let oversized = " ".repeat(1024 * 1024);
    let response = raw_request(
        srv.addr,
        &format!(
            "POST /v1/scope/job_context HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{oversized}",
            oversized.len()
        ),
    )
    .await;
    assert_eq!(status(&response), 413, "{response}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn child_artifact_reads_are_hardened() {
    const TOKEN: &str = "AKIAABCDEFGHIJKLMNOP";

    // A symlink planted in the artifact directory must not be followed.
    let srv = start_server(Some(scope_config())).await;
    let (parent, child_id, epoch, child_epoch) = artifact_setup(&srv).await;
    srv.store
        .record_artifact(&child_id, child_epoch, ArtifactName::ReportJson, 4)
        .unwrap();
    let dir = srv.artifact_dir.join(child_id.as_str());
    std::fs::create_dir_all(&dir).unwrap();
    let outside = srv.artifact_dir.join("outside-secret.txt");
    std::fs::write(&outside, b"secret-from-outside").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("report.json")).unwrap();
    let request = ScopeRequest::new(parent.clone(), epoch, ScopeAction::GetChildArtifact)
        .with_payload(ScopePayload {
            child: Some(child_id.clone()),
            artifact: Some("report.json".to_owned()),
            ..ScopePayload::default()
        });
    let resp = call_scope(&srv, "get_child_artifact", Some(AGENT_A_TOKEN), &request).await;
    assert_eq!(status(&resp), 404, "{resp}");
    assert!(!body(&resp).contains("secret-from-outside"));
    let _ = srv.shutdown.send(());

    // `[artifacts] scrub = false` must not silently serve raw bytes to a harness.
    let no_scrub = ArtifactsConfig {
        scrub: false,
        extra_rules: Vec::new(),
    };
    let srv =
        start_server_with_artifacts(Some(scope_config()), false, 6, 60, Some(&no_scrub)).await;
    let (parent, child_id, epoch, child_epoch) = artifact_setup(&srv).await;
    srv.store
        .record_artifact(&child_id, child_epoch, ArtifactName::ReportJson, 4)
        .unwrap();
    let dir = srv.artifact_dir.join(child_id.as_str());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("report.json"), format!("value={TOKEN}")).unwrap();
    let request = ScopeRequest::new(parent.clone(), epoch, ScopeAction::GetChildArtifact)
        .with_payload(ScopePayload {
            child: Some(child_id.clone()),
            artifact: Some("report.json".to_owned()),
            ..ScopePayload::default()
        });
    let resp = call_scope(&srv, "get_child_artifact", Some(AGENT_A_TOKEN), &request).await;
    assert_eq!(status(&resp), 404, "{resp}");
    assert!(!body(&resp).contains(TOKEN));
    let _ = srv.shutdown.send(());

    // On-disk bytes are re-scrubbed at read time, even if they predate the scrub rules.
    let srv = start_server(Some(scope_config())).await;
    let (parent, child_id, epoch, child_epoch) = artifact_setup(&srv).await;
    srv.store
        .record_artifact(&child_id, child_epoch, ArtifactName::ReportJson, 4)
        .unwrap();
    let dir = srv.artifact_dir.join(child_id.as_str());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("report.json"), format!("value={TOKEN}")).unwrap();
    let request = ScopeRequest::new(parent, epoch, ScopeAction::GetChildArtifact).with_payload(
        ScopePayload {
            child: Some(child_id),
            artifact: Some("report.json".to_owned()),
            ..ScopePayload::default()
        },
    );
    let resp = call_scope(&srv, "get_child_artifact", Some(AGENT_A_TOKEN), &request).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let value = json_body(&resp)["result"]["artifact"].clone();
    assert_eq!(value["scrubbed"], true, "{resp}");
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("[scrubbed:token_prefix]"), "{content}");
    assert!(!content.contains(TOKEN), "{content}");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn expired_leases_and_stale_epochs_are_refused() {
    let srv = start_server_with(Some(scope_config()), false, 1, 60).await;
    let (_, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let context = ScopeRequest::new(parent.clone(), epoch, ScopeAction::JobContext);

    let ok = call_scope(&srv, "job_context", Some(AGENT_A_TOKEN), &context).await;
    assert_eq!(status(&ok), 200, "{ok}");

    let mut stale_epochs = vec![epoch + 1];
    if epoch > 1 {
        stale_epochs.push(epoch - 1);
    }
    for stale in stale_epochs {
        let request = ScopeRequest::new(parent.clone(), stale, ScopeAction::JobContext);
        let resp = call_scope(&srv, "job_context", Some(AGENT_A_TOKEN), &request).await;
        assert_eq!(status(&resp), 409, "{resp}");
        assert_eq!(error_code(&resp), "stale_epoch");
    }

    // The lease deadline is checked in the store, not just the epoch field.
    tokio::time::sleep(Duration::from_millis(1_150)).await;
    let resp = call_scope(&srv, "job_context", Some(AGENT_A_TOKEN), &context).await;
    assert_eq!(status(&resp), 409, "{resp}");
    assert_eq!(error_code(&resp), "stale_epoch");
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn children_are_isolated_and_parent_cancel_cascades() {
    let srv = start_server(Some(scope_config())).await;
    let mut spec = sample_spec();
    spec.task = "PARENT_ONLY_SECRET".to_owned();
    let (_, parent, epoch) = lease_spec(&srv, &spec).await;

    let first = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("first child".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let first_id = lu_proto::JobId::parse(first["job_id"].as_str().unwrap()).unwrap();
    let (_, child_epoch) =
        lease_as(&srv, AGENT_B_TOKEN, "node-b", first_id.as_str(), &["p1"]).await;

    let own = scope_ok_as(
        &srv,
        AGENT_B_TOKEN,
        ScopeAction::JobContext,
        &first_id,
        child_epoch,
        None,
    )
    .await;
    assert_eq!(own["spec"]["task"], "first child");
    assert!(!own.to_string().contains("PARENT_ONLY_SECRET"));

    let second = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("second child".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let second_id = lu_proto::JobId::parse(second["job_id"].as_str().unwrap()).unwrap();
    let missing = lu_proto::JobId::new();

    for (action, target) in [
        (ScopeAction::GetChild, second_id.clone()),
        (ScopeAction::CancelChild, second_id.clone()),
        (ScopeAction::GetChildArtifact, second_id.clone()),
        (ScopeAction::GetChild, parent.clone()),
        (ScopeAction::CancelChild, parent.clone()),
        (ScopeAction::GetChild, missing),
    ] {
        let mut payload = ScopePayload {
            child: Some(target.clone()),
            ..ScopePayload::default()
        };
        if action == ScopeAction::GetChildArtifact {
            payload.artifact = Some("report.json".to_owned());
        }
        let request =
            ScopeRequest::new(first_id.clone(), child_epoch, action).with_payload(payload);
        let resp = call_scope(&srv, action.as_str(), Some(AGENT_B_TOKEN), &request).await;
        assert_eq!(status(&resp), 404, "{action} {target}: {resp}");
        assert_eq!(error_code(&resp), "not_found");
    }

    let grandchild = scope_ok_as(
        &srv,
        AGENT_B_TOKEN,
        ScopeAction::SubmitChild,
        &first_id,
        child_epoch,
        Some(ScopePayload {
            task: Some("grandchild".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let grandchild_id = lu_proto::JobId::parse(grandchild["job_id"].as_str().unwrap()).unwrap();
    assert_eq!(srv.store.get_job(&grandchild_id).unwrap().depth, 2);

    let request =
        ScopeRequest::new(parent, epoch, ScopeAction::CancelChild).with_payload(ScopePayload {
            child: Some(first_id.clone()),
            ..ScopePayload::default()
        });
    let resp = call_scope(&srv, "cancel_child", Some(AGENT_A_TOKEN), &request).await;
    assert_eq!(status(&resp), 200, "{resp}");
    assert_eq!(
        srv.store.get_job(&grandchild_id).unwrap().state,
        lu_proto::JobState::Cancelled
    );
    assert!(srv.store.get_job(&first_id).unwrap().cancel_requested);

    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn operator_cancel_can_keep_children() {
    let srv = start_server(Some(scope_config())).await;
    let (parent_id, parent, epoch) = lease_spec(&srv, &sample_spec()).await;
    let child = scope_ok(
        &srv,
        ScopeAction::SubmitChild,
        &parent,
        epoch,
        Some(ScopePayload {
            task: Some("kept child".to_owned()),
            ..ScopePayload::default()
        }),
    )
    .await;
    let child_id = lu_proto::JobId::parse(child["job_id"].as_str().unwrap()).unwrap();

    let req = format!(
        "POST /v1/jobs/{parent_id}/cancel?keep_children=1 HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"
    );
    let resp = raw_request(srv.addr, &req).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let child_record: JobRecord = srv.store.get_job(&child_id).unwrap();
    assert_eq!(child_record.state, lu_proto::JobState::Queued);
    assert!(!child_record.cancel_requested);
    let _ = srv.shutdown.send(());
}

#[tokio::test]
async fn audit_and_metrics_record_scope_actions_without_task_text() {
    let srv = start_server_with(Some(scope_config()), true, 6, 60).await;
    let secret = "TOP_SECRET_SCOPE_TASK_9F3A";
    let mut spec = sample_spec();
    spec.task = secret.to_owned();
    let (_, parent, epoch) = lease_spec(&srv, &spec).await;

    // Pre-lease failures are metered but never attributed to a job in the audit log.
    let context = ScopeRequest::new(parent.clone(), epoch, ScopeAction::JobContext);
    let unauthorized = call_scope(&srv, "job_context", Some("wrong-token"), &context).await;
    assert_eq!(status(&unauthorized), 401, "{unauthorized}");
    let unknown = call_scope(&srv, "not_a_route", Some(AGENT_A_TOKEN), &context).await;
    assert_eq!(status(&unknown), 404, "{unknown}");
    let mismatch = call_scope(&srv, "submit_child", Some(AGENT_A_TOKEN), &context).await;
    assert_eq!(status(&mismatch), 400, "{mismatch}");
    let bad_json = format!(
        "POST /v1/scope/job_context HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {AGENT_A_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
    );
    let bad_json = raw_request(srv.addr, &bad_json).await;
    assert_eq!(status(&bad_json), 400, "{bad_json}");

    let ok = call_scope(
        &srv,
        "job_context",
        Some(AGENT_A_TOKEN),
        &ScopeRequest::new(parent.clone(), epoch, ScopeAction::JobContext),
    )
    .await;
    assert_eq!(status(&ok), 200, "{ok}");
    let limited = call_scope(
        &srv,
        "submit_child",
        Some(AGENT_A_TOKEN),
        &ScopeRequest::new(parent.clone(), epoch, ScopeAction::SubmitChild).with_payload(
            ScopePayload {
                task: Some("x".repeat(1025)),
                ..ScopePayload::default()
            },
        ),
    )
    .await;
    assert_limit(&limited, "max_task_bytes");

    let audit_path = srv.audit_path.as_ref().unwrap();
    let mut audit = String::new();
    for _ in 0..50 {
        audit = std::fs::read_to_string(audit_path).unwrap_or_default();
        if audit.contains("\"scope_action\"") && audit.contains("\"limit_exceeded\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(audit.contains("\"scope_action\""), "{audit}");
    assert!(
        audit.contains(&format!("\"actor\":\"job:{parent}\"")),
        "{audit}"
    );
    assert!(audit.contains("\"source\":\"child_job\""), "{audit}");
    assert!(audit.contains("\"mode\":\"job_context\""), "{audit}");
    assert!(audit.contains("\"mode\":\"submit_child\""), "{audit}");
    assert!(audit.contains("\"outcome\":\"limit_exceeded\""), "{audit}");
    assert!(!audit.contains(secret), "task text in audit: {audit}");
    assert_eq!(srv.state.audit_write_errors(), 0);

    let req = format!(
        "GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"
    );
    let metrics = raw_request(srv.addr, &req).await;
    assert!(
        metrics.contains("lu_scope_requests_total{route=\"job_context\",result=\"ok\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains(
            "lu_scope_requests_total{route=\"submit_child\",result=\"limit_exceeded\"} 1"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains("lu_scope_limit_exceeded_total{limit=\"max_task_bytes\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics
            .contains("lu_scope_requests_total{route=\"job_context\",result=\"unauthorized\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("lu_scope_requests_total{route=\"unknown\",result=\"not_found\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("lu_scope_requests_total{route=\"job_context\",result=\"bad_request\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics
            .contains("lu_scope_requests_total{route=\"submit_child\",result=\"bad_request\"} 1"),
        "{metrics}"
    );
    assert!(!metrics.contains(secret));

    // One combined sweep: no task text or credential may appear in any operator-visible surface.
    let show = raw_request(
        srv.addr,
        &format!(
            "GET /v1/jobs/{parent} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {OP_TOKEN}\r\n\r\n"
        ),
    )
    .await;
    let artifacts = read_tree(&srv.artifact_dir);
    for surface in [audit.as_str(), body(&metrics), artifacts.as_str()] {
        for needle in [secret, AGENT_A_TOKEN, OP_TOKEN, "wrong-token"] {
            assert!(
                !surface.contains(needle),
                "a secret leaked into the audit, metrics or artifacts"
            );
        }
    }
    // `lu show` is an operator view: task text is expected, credentials are not.
    for needle in [AGENT_A_TOKEN, OP_TOKEN, "wrong-token"] {
        assert!(!body(&show).contains(needle), "{needle} leaked in {show}");
    }
    let _ = srv.shutdown.send(());
}
