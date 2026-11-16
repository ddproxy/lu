use crate::auth::authenticate_operator;
use crate::error::ApiError;
use crate::routes::{
    json_ok, json_value_ok, parse_job_state, read_json, store_err, with_version, MAX_JSON_BODY,
};
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_proto::{ArtifactName, JobId, JobSpec, NodeName};
use lu_serve::PeerAddr;
use lu_store::{JobRecord, NodeRecord};
use serde_json::json;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

pub fn job_view(r: &JobRecord) -> serde_json::Value {
    let artifacts: Vec<&str> = r.artifacts.iter().map(|a| a.file_name()).collect();
    let failure_class = r.failure_class.map(|f| match f {
        lu_proto::FailureClass::Node => "node",
        lu_proto::FailureClass::Provider => "provider",
        lu_proto::FailureClass::Execution => "execution",
    });
    json!({
        "id": r.id.to_string(),
        "state": match r.state {
            lu_proto::JobState::Queued => "queued",
            lu_proto::JobState::Leased => "leased",
            lu_proto::JobState::Running => "running",
            lu_proto::JobState::Recovering => "recovering",
            lu_proto::JobState::NeedsInput => "needs_input",
            lu_proto::JobState::Done => "done",
            lu_proto::JobState::Failed => "failed",
            lu_proto::JobState::Cancelled => "cancelled",
        },
        "spec": &r.spec,
        "attempt": r.attempt,
        "node": r.node.as_ref().map(|n| n.to_string()),
        "epoch": r.epoch,
        "created_at": r.created_at,
        "updated_at": r.updated_at,
        "started_at": r.started_at,
        "finished_at": r.finished_at,
        "failure_class": failure_class,
        "summary": r.summary,
        "exit_code": r.exit_code,
        "artifacts": artifacts,
        "cancel_requested": r.cancel_requested,
    })
}

fn node_view(n: &NodeRecord) -> serde_json::Value {
    let harness = match n.heartbeat.harness {
        lu_proto::HarnessState::Idle => "idle",
        lu_proto::HarnessState::Busy => "busy",
    };
    json!({
        "name": n.name.to_string(),
        "last_seen": n.last_seen,
        "online": n.online,
        "draining": n.draining,
        "harness": harness,
        "providers": &n.heartbeat.providers,
        "current_job": n.current_job.as_ref().map(|j| j.to_string()),
        "cordons": &n.cordons,
        "reiki": n.heartbeat.reiki,
    })
}

pub async fn create_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    let _ = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(o) => o,
        Err(e) => return with_version(e.into_response()),
    };
    let spec: JobSpec = match read_json(req, MAX_JSON_BODY).await {
        Ok(s) => s,
        Err(e) => return with_version(e.into_response()),
    };
    let now = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.create_job(&spec, now)
    })
    .await;
    let (rec, created) = match res {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return with_version(store_err(e).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    if created {
        state.waiters.notify_work();
        with_version(json_ok(StatusCode::CREATED, &job_view(&rec)))
    } else {
        with_version(json_ok(StatusCode::OK, &job_view(&rec)))
    }
}

pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let q = crate::routes::parse_query(&uri);
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200)
        .min(200);
    let state_filter = match q.get("state") {
        Some(s) if !s.is_empty() => match parse_job_state(s) {
            Some(st) => Some(st),
            None => return with_version(ApiError::bad_request("invalid state").into_response()),
        },
        _ => None,
    };
    let after = match q.get("after") {
        Some(a) if !a.is_empty() => match JobId::parse(a) {
            Ok(id) => Some(id),
            Err(_) => return with_version(ApiError::bad_request("invalid cursor").into_response()),
        },
        _ => None,
    };
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.list_jobs(state_filter, limit, after.as_ref())
    })
    .await;
    let jobs = match res {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return with_version(store_err(e).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let next = jobs.last().map(|j| j.id.to_string());
    let body = json!({
        "jobs": jobs.iter().map(job_view).collect::<Vec<_>>(),
        "next": next,
    });
    with_version(json_value_ok(StatusCode::OK, body))
}

pub async fn get_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.get_job(&job_id)
    })
    .await;
    match res {
        Ok(Ok(rec)) => with_version(json_ok(StatusCode::OK, &job_view(&rec))),
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

pub async fn cancel_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.cancel_job(&job_id, n)
    })
    .await;
    match res {
        Ok(Ok(rec)) => {
            state.waiters.notify_job(rec.id.as_ref());
            state.waiters.notify_work();
            with_version(json_ok(StatusCode::OK, &job_view(&rec)))
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

fn artifact_name_from_str(name: &str) -> Option<ArtifactName> {
    match name {
        "diff.patch" => Some(ArtifactName::DiffPatch),
        "run.log" => Some(ArtifactName::RunLog),
        "report.json" => Some(ArtifactName::ReportJson),
        _ => None,
    }
}

pub async fn get_artifact(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let Some(art) = artifact_name_from_str(&name) else {
        return with_version(ApiError::not_found("not found").into_response());
    };
    // ensure job exists? contract says 404 until stored - we can check file existence primarily.
    let path = state
        .artifacts_dir
        .join(job_id.to_string())
        .join(art.file_name());
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ct = if matches!(art, ArtifactName::ReportJson) {
                "application/json"
            } else {
                "application/octet-stream"
            };
            let res = (StatusCode::OK, [(header::CONTENT_TYPE, ct)], bytes).into_response();
            with_version(res)
        }
        Err(_) => with_version(ApiError::not_found("not found").into_response()),
    }
}

pub async fn get_logs(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let q = crate::routes::parse_query(&uri);
    let offset = q
        .get("offset")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let wait_secs = q
        .get("wait")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        .min(25);
    let job_id_s = job_id.to_string();
    // verify job exists
    let rec0 = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if wait_secs > 0 && offset >= rec0.log_len && !rec0.state.is_terminal() {
        if let Some((_permit, group)) = state.waiters.try_acquire_log(&job_id_s) {
            let _ =
                crate::wait::Waiters::wait_job(&group, std::time::Duration::from_secs(wait_secs))
                    .await;
        }
        // if cannot acquire, fall through immediately (bounded)
    }
    let rec = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    let path = state.artifacts_dir.join(job_id.to_string()).join("run.log");
    let data = if offset >= rec.log_len {
        Vec::new()
    } else {
        match tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let mut f = fs::File::open(&path)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await
        {
            Ok(Ok(b)) => b,
            _ => Vec::new(),
        }
    };
    let mut res = (StatusCode::OK, [(header::CONTENT_TYPE, "text/plain")], data).into_response();
    res.headers_mut().insert(
        "x-lu-log-length",
        header::HeaderValue::from_str(&rec.log_len.to_string())
            .unwrap_or(header::HeaderValue::from_static("0")),
    );
    let st = match rec.state {
        lu_proto::JobState::Queued => "queued",
        lu_proto::JobState::Leased => "leased",
        lu_proto::JobState::Running => "running",
        lu_proto::JobState::Recovering => "recovering",
        lu_proto::JobState::NeedsInput => "needs_input",
        lu_proto::JobState::Done => "done",
        lu_proto::JobState::Failed => "failed",
        lu_proto::JobState::Cancelled => "cancelled",
    };
    res.headers_mut().insert(
        "x-lu-state",
        header::HeaderValue::from_str(st).unwrap_or(header::HeaderValue::from_static("queued")),
    );
    with_version(res)
}

pub async fn list_nodes(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || store.nodes(n, lease_ms)
    })
    .await;
    match res {
        Ok(Ok(nodes)) => with_version(json_ok(
            StatusCode::OK,
            &nodes.iter().map(node_view).collect::<Vec<_>>(),
        )),
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

pub async fn set_drain(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
    draining: bool,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let node = match NodeName::parse(&name) {
        Ok(n) => n,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.set_draining(&node, draining)
    })
    .await;
    match res {
        Ok(Ok(())) => {
            state.waiters.notify_work();
            with_version(
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    "{}",
                )
                    .into_response(),
            )
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

pub async fn drain_node(
    s: State<Arc<AppState>>,
    p: axum::Extension<PeerAddr>,
    h: HeaderMap,
    path: Path<String>,
) -> Response {
    set_drain(s, p, h, path, true).await
}
pub async fn undrain_node(
    s: State<Arc<AppState>>,
    p: axum::Extension<PeerAddr>,
    h: HeaderMap,
    path: Path<String>,
) -> Response {
    set_drain(s, p, h, path, false).await
}
