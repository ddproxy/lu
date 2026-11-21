use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use lu_client::{
    ApiError, Client, ClientOptions, Controller, DependencyView, LogPage, PollRequest,
};
use lu_proto::{
    AgentEvent, ArtifactName, Completion, FailureClass, JobId, JobSpec, JobState, LogChunk,
    NodeName, Outcome,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

const TOKEN: &str = "operator-token-do-not-print";

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

    fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.push((key.to_owned(), value.to_owned()));
        self
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

fn client(server: &TestServer) -> Client {
    Client::new(&server.url, TOKEN, None).unwrap()
}

fn fast_options() -> ClientOptions {
    ClientOptions {
        backoff: Duration::from_millis(1),
        max_attempts: 3,
        ..ClientOptions::default()
    }
}

fn job_id() -> JobId {
    JobId::new()
}

fn job_json(id: &JobId, state: JobState) -> Value {
    json!({
        "id": id,
        "state": state,
        "spec": {"repo": "/srv/repo", "git_ref": "main", "task": "go"},
        "attempt": 1,
        "created_at": 10,
        "started_at": 11,
        "failure_class": "execution",
        "summary": "failed tests\n\u{1b}[31m"
    })
}

fn spec() -> JobSpec {
    JobSpec {
        repo: "https://example.net/repo.git".to_owned(),
        git_ref: "main".to_owned(),
        base_commit: None,
        task: "run the tests".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 60,
        idle_timeout_secs: None,
        idempotency_key: Some("ticket:42".to_owned()),
        after: Vec::new(),
        stack_dependencies: true,
        collect: None,
        include_failed: false,
        on_conflict: Default::default(),
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        on_stuck: None,
        stuck_after_secs: None,
        priority: lu_proto::DEFAULT_PRIORITY,
        group: None,
        fallbacks: Vec::new(),
        expect: None,
    }
}

fn completion(id: JobId) -> Completion {
    Completion {
        job_id: id,
        epoch: 7,
        stuck_mode: None,
        outcome: Outcome::Failed(FailureClass::Execution),
        exit_code: Some(2),
        summary: Some("bad".to_owned()),
        retry_after_secs: None,
        expect_failed: false,
    }
}

#[tokio::test]
async fn submit_posts_the_spec_and_keeps_the_idempotency_key() {
    let id = job_id();
    let body = job_json(&id, JobState::Queued);
    let server = start(move |_| Reply::json(&body)).await;
    let client = client(&server);
    let view = client.submit(&spec()).await.unwrap();
    assert_eq!(view.id, id);
    let records = server.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].path, "/v1/jobs");
    assert_eq!(
        records[0].header("authorization"),
        Some("Bearer operator-token-do-not-print")
    );
    assert_eq!(records[0].header("idempotency-key"), Some("ticket:42"));
    let sent: JobSpec = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(sent, spec());
}

#[tokio::test]
async fn submit_carries_dependencies_both_ways() {
    let id = job_id();
    let dep = job_id();
    let body = json!({
        "id": id,
        "state": "blocked",
        "after": [dep],
        "waiting_on": [dep],
    });
    let server = start(move |_| Reply::json(&body)).await;
    let mut spec = spec();
    spec.after = vec![dep.clone()];
    let view = client(&server).submit(&spec).await.unwrap();
    assert_eq!(view.state, JobState::Blocked);
    assert_eq!(view.after, vec![dep.clone()]);
    assert_eq!(view.waiting_on, vec![dep.clone()]);
    let records = server.records.lock().unwrap();
    let sent: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(sent["after"], json!([dep]));
}

#[tokio::test]
async fn list_jobs_sends_the_filters_and_reads_a_page() {
    let id = job_id();
    let body = json!({"jobs": [job_json(&id, JobState::Running)]});
    let server = start(move |_| Reply::json(&body)).await;
    let client = client(&server);
    let page = client
        .list_jobs(Some(JobState::Running), Some(5))
        .await
        .unwrap();
    assert_eq!(page.jobs.len(), 1);
    assert_eq!(page.jobs[0].id, id);
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].path, "/v1/jobs");
    assert_eq!(records[0].query.as_deref(), Some("state=running&limit=5"));
}

#[tokio::test]
async fn job_by_key_sends_the_key_and_reads_zero_or_one_job() {
    let id = job_id();
    let found = json!({"jobs": [job_json(&id, JobState::Done)], "next": null});
    let missing = json!({"jobs": [], "next": null});
    let server = start(move |record| {
        assert_eq!(record.method, "GET");
        assert_eq!(record.path, "/v1/jobs");
        if record
            .query
            .as_deref()
            .is_some_and(|q| q.contains("ticket"))
        {
            Reply::json(&found)
        } else {
            Reply::json(&missing)
        }
    })
    .await;
    let c = client(&server);
    let view = c.job_by_key("ticket:42").await.unwrap().unwrap();
    assert_eq!(view.id, id);
    assert_eq!(view.state, JobState::Done);
    assert!(c.job_by_key("unknown").await.unwrap().is_none());
    let records = server.records.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert!(
        records[0]
            .query
            .as_deref()
            .is_some_and(|q| q.starts_with("key=") && q.contains("ticket")),
        "{:?}",
        records[0].query
    );
    assert_eq!(records[1].query.as_deref(), Some("key=unknown"));
}

#[tokio::test]
async fn list_jobs_reads_the_documented_envelope() {
    let id = job_id();
    let body = json!({"jobs": [job_json(&id, JobState::Done)], "next": null});
    let server = start(move |_| Reply::json(&body)).await;
    let page = client(&server).list_jobs(None, None).await.unwrap();
    assert_eq!(page.jobs[0].id, id);
}

#[tokio::test]
async fn get_job_uses_the_job_route() {
    let id = job_id();
    let body = job_json(&id, JobState::Running);
    let server = start(move |_| Reply::json(&body)).await;
    let view = client(&server).get_job(&id).await.unwrap();
    assert_eq!(view.node, None);
    assert_eq!(view.failure_class, Some(FailureClass::Execution));
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].path, format!("/v1/jobs/{id}"));
    assert_eq!(records[0].method, "GET");
}

#[tokio::test]
async fn logs_sends_offset_and_wait_and_reads_the_next_offset() {
    let id = job_id();
    let body = json!({"data": "hello\n", "next": 12, "state": "running"});
    let server = start(move |_| Reply::json(&body)).await;
    let page: LogPage = client(&server).logs(&id, 3, Some(25)).await.unwrap();
    assert_eq!(page.data, "hello\n");
    assert_eq!(page.offset, 12);
    assert_eq!(page.state, Some(JobState::Running));
    assert!(!page.terminal);
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].path, format!("/v1/jobs/{id}/logs"));
    assert_eq!(records[0].query.as_deref(), Some("offset=3&wait=25"));
}

#[tokio::test]
async fn logs_accept_plain_text_and_advance_by_the_body_length() {
    let id = job_id();
    let server = start(|_| Reply::ok("abc".as_bytes().to_vec())).await;
    let page = client(&server).logs(&id, 10, None).await.unwrap();
    assert_eq!(page.data, "abc");
    assert_eq!(page.offset, 13);
}

#[tokio::test]
async fn fetch_artifact_uses_the_fixed_name_and_returns_bytes() {
    let id = job_id();
    let server = start(|record| {
        assert!(record.path.ends_with("/artifacts/diff.patch"));
        Reply::ok(b"diff --git".to_vec())
    })
    .await;
    let bytes = client(&server)
        .fetch_artifact(&id, ArtifactName::DiffPatch)
        .await
        .unwrap();
    assert_eq!(bytes, b"diff --git");
}

#[tokio::test]
async fn an_unknown_job_makes_exactly_one_request() {
    let id = job_id();
    let server = start(|_| Reply::status(404)).await;
    let error = client(&server).get_job(&id).await.unwrap_err();
    assert_eq!(error, ApiError::NotFound);
    let records = server.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[0].path, format!("/v1/jobs/{id}"));
}

#[tokio::test]
async fn missing_artifacts_are_not_found() {
    let id = job_id();
    let server = start(|_| Reply::status(404)).await;
    let error = client(&server)
        .fetch_artifact(&id, ArtifactName::ReportJson)
        .await
        .unwrap_err();
    assert_eq!(error, ApiError::NotFound);
}

#[tokio::test]
async fn cancel_accepts_an_empty_reply() {
    let id = job_id();
    let server = start(|_| Reply::status(204)).await;
    let view = client(&server).cancel(&id).await.unwrap();
    assert_eq!(view.id, id);
    assert_eq!(view.state, JobState::Cancelled);
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].path, format!("/v1/jobs/{id}/cancel"));
}

#[tokio::test]
async fn nodes_reads_a_bare_list() {
    let bare = json!([{"name": "node-a", "harness": "idle", "draining": false}]);
    let server = start(move |_| Reply::json(&bare)).await;
    let nodes = client(&server).nodes().await.unwrap();
    assert_eq!(nodes.nodes[0].name.as_str(), "node-a");
    assert_eq!(nodes.nodes[0].harness, Some(lu_proto::HarnessState::Idle));
}

#[tokio::test]
async fn repos_and_sync_read_the_controller_replies() {
    let repos = json!({"repos": [
        {"name": "mirror", "url": "https://example.net/a.git", "synced_at": 9, "heads": {"main": "abc"}},
        {"name": "idle", "url": "https://example.net/b.git"}
    ]});
    let synced = json!({"name": "mirror", "synced_at": 10, "heads": {"main": "abc"}});
    let server = start(move |record| {
        if record.path == "/v1/repos" {
            Reply::json(&repos)
        } else {
            Reply::json(&synced)
        }
    })
    .await;
    let client = client(&server);
    let list = client.repos().await.unwrap();
    assert_eq!(list.repos.len(), 2);
    assert_eq!(list.repos[0].name, "mirror");
    assert_eq!(list.repos[0].synced_at, Some(9));
    assert_eq!(
        list.repos[0].heads.get("main").map(String::as_str),
        Some("abc")
    );
    assert_eq!(list.repos[1].synced_at, None);

    let repo = client.sync_repo("mirror").await.unwrap();
    assert_eq!(repo.name, "mirror");
    assert_eq!(repo.synced_at, Some(10));
    assert_eq!(repo.heads.get("main").map(String::as_str), Some("abc"));

    let error = client.sync_repo("../escape").await.unwrap_err();
    assert!(matches!(error, ApiError::Config { .. }), "{error:?}");

    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[0].path, "/v1/repos");
    assert_eq!(records[1].method, "POST");
    assert_eq!(records[1].path, "/v1/repos/mirror/sync");
    assert_eq!(records.len(), 2, "an invalid name must not be sent");
}

#[tokio::test]
async fn drain_and_undrain_hit_the_node_actions() {
    let node = NodeName::parse("node-a").unwrap();
    let reply = json!({"name": "node-a", "draining": true});
    let server = start(move |_| Reply::json(&reply)).await;
    let client = client(&server);
    let view = client.drain(&node).await.unwrap();
    assert!(view.draining);
    let _ = client.undrain(&node).await.unwrap();
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].path, "/v1/nodes/node-a/drain");
    assert_eq!(records[1].path, "/v1/nodes/node-a/undrain");
}

#[tokio::test]
async fn every_agent_call_uses_the_documented_path_and_epoch() {
    let id = job_id();
    let poll_id = id.clone();
    let server = start(move |record| match record.path.as_str() {
        path if path.ends_with("/poll") => {
            Reply::json(&json!([{"kind":"cancel","job_id": poll_id, "epoch": 7}]))
        }
        path if path.ends_with("/renew") => Reply::json(&json!({"cancel": true})),
        _ => Reply::status(204),
    })
    .await;
    let client = client(&server);
    let mut request = PollRequest::for_node(NodeName::parse("node-a").unwrap());
    request.harness = Some(lu_proto::HarnessState::Idle);
    let events = client.poll(&request).await.unwrap();
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::Cancel { epoch: 7, .. }]
    ));

    client.start(&id, 7).await.unwrap();
    assert!(client.renew(&id, 7).await.unwrap());
    client
        .append_log(&LogChunk {
            job_id: id.clone(),
            epoch: 7,
            seq: 1,
            data: "line\n".to_owned(),
        })
        .await
        .unwrap();
    client
        .put_artifact(&id, 7, ArtifactName::RunLog, b"log")
        .await
        .unwrap();
    client.complete(&completion(id.clone())).await.unwrap();

    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
    assert_eq!(paths[0], "/v1/agent/poll");
    assert_eq!(paths[1], format!("/v1/agent/jobs/{id}/start"));
    assert_eq!(paths[2], format!("/v1/agent/jobs/{id}/renew"));
    assert_eq!(paths[3], format!("/v1/agent/jobs/{id}/log"));
    assert_eq!(paths[4], format!("/v1/agent/jobs/{id}/artifacts/run.log"));
    assert_eq!(paths[5], format!("/v1/agent/jobs/{id}/complete"));
    let poll: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(poll["heartbeat"]["node"], json!("node-a"));
    assert_eq!(poll["heartbeat"]["harness"], json!("idle"));
    assert_eq!(poll["wait_secs"], json!(0));
    let start: Value = serde_json::from_slice(&records[1].body).unwrap();
    assert_eq!(
        start,
        json!({"epoch": 7}),
        "no field the controller would reject"
    );
    assert_eq!(records[3].query.as_deref(), Some("epoch=7&offset=1"));
    assert_eq!(records[3].body, b"line\n", "the log goes as raw bytes");
    assert_eq!(records[4].query.as_deref(), Some("epoch=7"));
    assert_eq!(records[4].body, b"log");
    let sent: Value = serde_json::from_slice(&records[5].body).unwrap();
    assert_eq!(sent["job_id"], json!(id));
    assert_eq!(sent["epoch"], json!(7));
}

#[tokio::test]
async fn a_poll_without_a_node_or_harness_is_refused_before_any_request() {
    let server = start(|_| Reply::status(204)).await;
    let error = client(&server)
        .poll(&PollRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::BadRequest { .. }), "{error:?}");
    assert!(server.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_empty_poll_is_no_assignment() {
    let server = start(|_| Reply::status(204)).await;
    let mut request = PollRequest::for_node(NodeName::parse("node-a").unwrap());
    request.harness = Some(lu_proto::HarnessState::Idle);
    let events = client(&server).poll(&request).await.unwrap();
    assert!(events.is_empty());
}

#[tokio::test]
async fn stale_epoch_conflict_and_unauthorized_are_distinct() {
    let id = job_id();
    let gone = start(|_| Reply::status(410)).await;
    assert_eq!(
        client(&gone).get_job(&id).await.unwrap_err(),
        ApiError::StaleEpoch
    );

    let conflict = start(|_| {
        let mut reply = Reply::json(&json!({"error":"conflict","length":17}));
        reply.status = 409;
        reply
    })
    .await;
    assert_eq!(
        client(&conflict).get_job(&id).await.unwrap_err(),
        ApiError::Conflict { length: 17 }
    );

    let denied = start(|_| Reply::status(401)).await;
    assert_eq!(
        client(&denied).get_job(&id).await.unwrap_err(),
        ApiError::Unauthorized
    );
}

#[tokio::test]
async fn a_redirect_is_refused_and_not_followed() {
    let id = job_id();
    let location = format!("http://{}", "example.net");
    let server =
        start(move |_| Reply::status(302).with_header("location", &format!("{location}/steal")))
            .await;
    let error = client(&server).get_job(&id).await.unwrap_err();
    assert!(
        matches!(error, ApiError::Redirect { status: 302 }),
        "{error:?}"
    );
    assert_eq!(server.records.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_json_body_has_a_hard_cap() {
    let id = job_id();
    let server = start(|_| Reply::ok(vec![b'x'; 256])).await;
    let options = ClientOptions {
        max_json_bytes: 64,
        ..fast_options()
    };
    let client = Client::new_with_options(&server.url, TOKEN, options).unwrap();
    let error = client.get_job(&id).await.unwrap_err();
    assert_eq!(error, ApiError::TooLarge { limit: 64 });
}

#[tokio::test]
async fn debug_and_errors_never_contain_the_token() {
    let id = job_id();
    let echo = format!("token was {TOKEN}");
    let server = start(move |_| {
        let mut reply = Reply::json(&json!({"error":"denied","echo": echo}));
        reply.status = 403;
        reply
    })
    .await;
    let client = client(&server);
    assert!(!format!("{client:?}").contains(TOKEN));
    let error = client.get_job(&id).await.unwrap_err();
    assert!(!format!("{error:?}").contains(TOKEN), "{error:?}");
    assert!(!error.to_string().contains(TOKEN), "{error}");
}

#[tokio::test]
async fn a_hostile_success_body_cannot_leak_the_token() {
    let id = job_id();
    let body = format!("this is not json and it mentions {TOKEN}");
    let server = start(move |_| Reply::ok(body.clone().into_bytes())).await;
    let error = client(&server).get_job(&id).await.unwrap_err();
    assert!(matches!(error, ApiError::Decode { .. }), "{error:?}");
    assert!(!format!("{error:?}").contains(TOKEN), "{error:?}");
    assert!(!error.to_string().contains(TOKEN), "{error}");
}

#[tokio::test]
async fn only_idempotent_gets_are_retried() {
    let id = job_id();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let body = job_json(&id, JobState::Queued);
    let server = start(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::status(500)
        } else {
            Reply::json(&body)
        }
    })
    .await;
    let client = Client::new_with_options(&server.url, TOKEN, fast_options()).unwrap();
    assert!(client.get_job(&id).await.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let server = start(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Reply::status(500)
    })
    .await;
    let client = Client::new_with_options(&server.url, TOKEN, fast_options()).unwrap();
    assert!(client.submit(&spec()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn server_errors_and_rate_limits_are_reported() {
    let id = job_id();
    let options = ClientOptions {
        max_attempts: 1,
        ..fast_options()
    };
    let server = start(|_| Reply::status(500)).await;
    let client = Client::new_with_options(&server.url, TOKEN, options.clone()).unwrap();
    assert!(matches!(
        client.get_job(&id).await.unwrap_err(),
        ApiError::Server { status: 500, .. }
    ));

    let server = start(|_| Reply::status(429).with_header("retry-after", "9")).await;
    let client = Client::new_with_options(&server.url, TOKEN, options).unwrap();
    assert_eq!(
        client.get_job(&id).await.unwrap_err(),
        ApiError::TooManyRequests {
            retry_after: Some(9)
        }
    );
}

#[tokio::test]
async fn a_network_failure_is_a_network_error() {
    let client = Client::new("http://127.0.0.1:1", TOKEN, None).unwrap();
    let error = client.get_job(&job_id()).await.unwrap_err();
    assert!(matches!(error, ApiError::Network { .. }), "{error:?}");
}

#[test]
fn urls_are_checked_before_any_request() {
    assert!(Client::new("http://example.net", TOKEN, None).is_err());
    assert!(Client::new("http://127.0.0.1:7600", TOKEN, None).is_ok());
    assert!(Client::new("https://example.net", TOKEN, None).is_ok());
    assert!(Client::new("https://user@example.net", TOKEN, None).is_err());
    assert!(Client::new("https://example.net?x=1", TOKEN, None).is_err());
    assert!(Client::new("https://example.net#x", TOKEN, None).is_err());
    assert!(Client::new("ftp://example.net", TOKEN, None).is_err());
    assert!(Client::new("https://example.net", "bad\ntoken", None).is_err());
}

struct FakeController;

impl Controller for FakeController {
    fn poll<'a>(
        &'a self,
        _request: &'a PollRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<AgentEvent>, ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn start<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn renew<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(false) })
    }

    fn append_log<'a>(
        &'a self,
        _chunk: &'a LogChunk,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn put_artifact<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
        _name: ArtifactName,
        _bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn deps<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DependencyView>, ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn dep_diff<'a>(
        &'a self,
        _id: &'a JobId,
        _dep: &'a JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn complete<'a>(
        &'a self,
        _completion: &'a Completion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn the_controller_trait_can_be_faked() {
    let fake: Box<dyn Controller> = Box::new(FakeController);
    assert!(fake.poll(&PollRequest::default()).await.unwrap().is_empty());
    fake.start(&job_id(), 1).await.unwrap();
}

#[tokio::test]
async fn a_text_log_page_reports_the_job_state_from_the_header() {
    let server = start(|_| {
        let mut reply = Reply::ok("tail\n");
        reply.headers = vec![
            ("content-type".to_owned(), "text/plain".to_owned()),
            ("x-lu-log-length".to_owned(), "15".to_owned()),
            ("x-lu-state".to_owned(), "done".to_owned()),
        ];
        reply
    })
    .await;
    let page = client(&server).logs(&job_id(), 10, None).await.unwrap();
    assert_eq!(page.data, "tail\n");
    assert_eq!(
        page.offset, 15,
        "the next read starts after what was returned"
    );
    assert_eq!(page.state, Some(lu_proto::JobState::Done));
    assert!(page.terminal);

    let running = start(|_| {
        let mut reply = Reply::ok("more");
        reply.headers = vec![("x-lu-state".to_owned(), "running".to_owned())];
        reply
    })
    .await;
    let page = client(&running).logs(&job_id(), 0, None).await.unwrap();
    assert!(!page.terminal);
}

#[tokio::test]
async fn drain_accepts_the_controllers_empty_object_reply() {
    let node = NodeName::parse("node-a").unwrap();
    let server = start(|_| Reply::json(&json!({}))).await;
    let view = client(&server).drain(&node).await.unwrap();
    assert_eq!(view.name.as_str(), "node-a");
}

#[tokio::test]
async fn job_base_fetches_the_commit_header_and_pack() {
    let id = job_id();
    let commit = "a".repeat(40);
    let expected_commit = commit.clone();
    let path = format!("/v1/agent/jobs/{id}/base");
    let pack = b"PACK\x00\x00\x00\x02".to_vec();
    let server = start(move |recorded| {
        assert_eq!(recorded.method, "GET");
        assert_eq!(recorded.path, path);
        assert_eq!(recorded.query.as_deref(), Some("epoch=7"));
        Reply::ok(pack.clone()).with_header("x-lu-base-commit", &commit)
    })
    .await;
    let got = client(&server).job_base(&id, 7).await.unwrap().unwrap();
    assert_eq!(got.0, expected_commit);
    assert_eq!(got.1, b"PACK\x00\x00\x00\x02");
    let records = server.records.lock().unwrap();
    assert_eq!(
        records[0].header("authorization"),
        Some("Bearer operator-token-do-not-print")
    );
}

#[tokio::test]
async fn job_base_treats_404_as_no_base() {
    let server = start(|_| Reply::status(404)).await;
    assert_eq!(client(&server).job_base(&job_id(), 3).await.unwrap(), None);
}

#[tokio::test]
async fn job_base_refuses_a_reply_without_a_commit_header() {
    let server = start(|_| Reply::ok(b"PACK")).await;
    let error = client(&server).job_base(&job_id(), 3).await.unwrap_err();
    assert!(matches!(error, ApiError::Decode { .. }), "{error:?}");
}
