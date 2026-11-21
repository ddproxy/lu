use crate::auth::authenticate_operator;
use crate::error::{ApiError, ErrorCode};
use crate::routes::{
    json_ok, json_value_ok, parse_job_state, read_raw_limited, store_err, with_version,
    MAX_JSON_BODY,
};
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_proto::{
    validate_idempotency_key, ArtifactName, CordonView, JobEdit, JobId, JobSpec, JobView, NodeName,
    NodeView,
};
use lu_serve::PeerAddr;
use lu_store::{
    missing_labels, waiting_reason_with_groups, Cordon, CordonSource, JobRecord, NodeRecord, Now,
};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

/// Notes for terminal jobs. An empty diff takes precedence over unmet expectations: it says the
/// run changed nothing at all, which is more specific than a missing expected change.
pub fn outcome_note(r: &JobRecord) -> Option<&'static str> {
    if r.state != lu_proto::JobState::Done {
        return None;
    }
    if !r.spec.allow_empty
        && r.artifacts.contains(&ArtifactName::DiffPatch)
        && r.diff_bytes == Some(0)
    {
        return Some("no_changes");
    }
    if r.expect_failed {
        return Some("expect_failed");
    }
    None
}

/// Stored timestamps are Unix seconds, which are never negative in practice; the wire views use
/// unsigned seconds.
fn seconds(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub fn job_view(r: &JobRecord) -> JobView {
    job_view_with(r, None)
}

fn job_view_with(r: &JobRecord, reason: Option<String>) -> JobView {
    JobView {
        id: r.id.clone(),
        state: r.state,
        spec: r.spec.clone(),
        attempt: r.attempt,
        node: r.node.clone(),
        assigned_provider: r.assigned_provider.clone(),
        assigned_model: r.assigned_model.clone(),
        epoch: r.epoch,
        created_at: seconds(r.created_at),
        updated_at: seconds(r.updated_at),
        started_at: r.started_at.map(seconds),
        finished_at: r.finished_at.map(seconds),
        failure_class: r.failure_class,
        summary: r.summary.clone(),
        exit_code: r.exit_code,
        artifacts: r
            .artifacts
            .iter()
            .map(|artifact| artifact.file_name().to_owned())
            .collect(),
        cancel_requested: r.cancel_requested,
        after: r.spec.after.clone(),
        waiting_on: r.waiting_on.clone(),
        waiting_reason: reason,
        outcome_note: outcome_note(r).map(str::to_owned),
        diff_bytes: r.state.is_terminal().then(|| r.diff_bytes.unwrap_or(0)),
        warnings: None,
    }
}

async fn all_nodes(state: &Arc<AppState>, at: Now) -> Result<Vec<NodeRecord>, ApiError> {
    let store = Arc::clone(&state.store);
    let lease_ms = state.lease_ms;
    match tokio::task::spawn_blocking(move || store.nodes(at, lease_ms)).await {
        Ok(Ok(nodes)) => Ok(nodes),
        Ok(Err(e)) => Err(store_err(e)),
        Err(_) => Err(ApiError::internal()),
    }
}

async fn group_counts(state: &Arc<AppState>) -> Result<HashMap<String, usize>, ApiError> {
    let store = Arc::clone(&state.store);
    tokio::task::spawn_blocking(move || store.group_counts())
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(store_err)
}

/// A view whose waiting reason includes the group cap, when the job has one.
async fn job_view_for(state: &Arc<AppState>, rec: &JobRecord) -> Result<JobView, ApiError> {
    let nodes = all_nodes(state, now()).await?;
    let counts = group_counts(state).await?;
    let reason = waiting_reason_with_groups(rec, &nodes, &counts, &state.group_limits);
    Ok(job_view_with(rec, reason))
}

fn job_views(
    jobs: &[JobRecord],
    nodes: &[NodeRecord],
    counts: &HashMap<String, usize>,
    limits: &HashMap<String, usize>,
) -> Vec<JobView> {
    jobs.iter()
        .map(|job| job_view_with(job, waiting_reason_with_groups(job, nodes, counts, limits)))
        .collect()
}

/// Turns a monotonic deadline into the wall-clock time displayed to operators.
pub fn cordon_until_wall(until_mono_ms: i64, at: Now) -> i64 {
    let remaining_ms = until_mono_ms.saturating_sub(at.mono_ms as i64);
    at.wall_unix.saturating_add(remaining_ms / 1000)
}

pub fn cordon_views(cordons: &[Cordon], at: Now) -> Vec<CordonView> {
    cordons
        .iter()
        .map(|c| {
            let (source, until, lifts_after_healthy_heartbeats) = match c.source {
                CordonSource::Reiki => ("reiki", None, Some(2)),
                CordonSource::Failure => (
                    "failure",
                    c.until_mono_ms.map(|until| cordon_until_wall(until, at)),
                    None,
                ),
            };
            CordonView {
                provider: c.provider.clone(),
                source: source.to_owned(),
                reason: c.reason.clone(),
                set_at: c.set_at_unix,
                until,
                lifts_after_healthy_heartbeats,
            }
        })
        .collect()
}

fn node_view(
    n: &NodeRecord,
    stats: Option<&lu_proto::NodeStats>,
    cordons: Vec<CordonView>,
) -> NodeView {
    let host = stats.and_then(|stats| stats.host.as_ref());
    let stuck = stats
        .map(|stats| stats.jobs.iter().filter(|job| job.stuck).count())
        .unwrap_or(0);
    NodeView {
        name: n.name.clone(),
        version: n.heartbeat.version.clone(),
        last_seen: seconds(n.last_seen),
        online: n.online,
        draining: n.draining,
        harness: Some(n.heartbeat.harness),
        providers: n.heartbeat.providers.clone(),
        provider_models: n.heartbeat.provider_models.clone(),
        labels: n.heartbeat.labels.clone(),
        label_sources: n.heartbeat.label_sources.clone(),
        taints: n.heartbeat.taints.clone(),
        current_job: n.current_job.clone(),
        cordons: n.cordons.clone(),
        cordon_details: cordons,
        reiki: n.heartbeat.reiki.clone(),
        load1: host.map(|host| host.load1),
        mem_available_kb: host.map(|host| host.mem_available_kb),
        mem_total_kb: host.map(|host| host.mem_total_kb),
        stuck: u64::try_from(stuck).unwrap_or(0),
    }
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
    let bytes = match read_raw_limited(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return with_version(ApiError::bad_request("invalid request body").into_response())
        }
    };
    let mut spec: JobSpec = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => return with_version(ApiError::unprocessable(e.to_string()).into_response()),
    };
    if let Some(repo) = state.mirror.repo_for_url(&spec.repo) {
        let name = repo.name.clone();
        let git_ref = spec.git_ref.clone();
        let mirror = Arc::clone(&state.mirror);
        let resolved =
            tokio::task::spawn_blocking(move || mirror.ensure_resolved(&name, &git_ref)).await;
        match resolved {
            Ok(Ok(commit)) => spec.base_commit = Some(commit),
            Ok(Err(e)) if e.is_unknown_ref() => {
                return with_version(ApiError::unprocessable(e.to_string()).into_response())
            }
            Ok(Err(e)) => {
                return with_version(
                    ApiError::new(
                        ErrorCode::Internal,
                        format!("could not mirror {}: {e}", spec.repo),
                    )
                    .into_response(),
                )
            }
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    }
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
    let nodes = match all_nodes(&state, now).await {
        Ok(nodes) => nodes,
        Err(error) => return with_version(error.into_response()),
    };
    let warnings: Vec<String> = missing_labels(&rec.spec.requires, &nodes)
        .into_iter()
        .map(|label| format!("no node with label {label}"))
        .collect();
    let mut body = match job_view_for(&state, &rec).await {
        Ok(view) => view,
        Err(error) => return with_version(error.into_response()),
    };
    body.warnings = Some(warnings);
    if created {
        state.waiters.notify_work();
        with_version(json_ok(StatusCode::CREATED, &body))
    } else {
        with_version(json_ok(StatusCode::OK, &body))
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
    // `key` is a different lookup on the same route, not an extra list filter.
    if let Some(key) = q.get("key") {
        if let Err(error) = validate_idempotency_key(key) {
            return with_version(ApiError::bad_request(error.to_string()).into_response());
        }
        let key = key.clone();
        let res = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            move || store.job_by_key(&key)
        })
        .await;
        let rec = match res {
            Ok(rec) => rec,
            Err(_) => return with_version(ApiError::internal().into_response()),
        };
        let nodes = match all_nodes(&state, now()).await {
            Ok(nodes) => nodes,
            Err(error) => return with_version(error.into_response()),
        };
        let counts = match group_counts(&state).await {
            Ok(counts) => counts,
            Err(error) => return with_version(error.into_response()),
        };
        let jobs: Vec<JobRecord> = rec.into_iter().collect();
        let body = json!({
            "jobs": job_views(&jobs, &nodes, &counts, &state.group_limits),
            "next": null,
        });
        return with_version(json_value_ok(StatusCode::OK, body));
    }
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
    let nodes = match all_nodes(&state, now()).await {
        Ok(nodes) => nodes,
        Err(error) => return with_version(error.into_response()),
    };
    let counts = match group_counts(&state).await {
        Ok(counts) => counts,
        Err(error) => return with_version(error.into_response()),
    };
    let next = jobs.last().map(|j| j.id.to_string());
    let body = json!({
        "jobs": job_views(&jobs, &nodes, &counts, &state.group_limits),
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
        Ok(Ok(rec)) => match job_view_for(&state, &rec).await {
            Ok(view) => with_version(json_ok(StatusCode::OK, &view)),
            Err(error) => with_version(error.into_response()),
        },
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
            match job_view_for(&state, &rec).await {
                Ok(view) => with_version(json_ok(StatusCode::OK, &view)),
                Err(error) => with_version(error.into_response()),
            }
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

pub async fn edit_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let bytes = match read_raw_limited(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    let edit: JobEdit = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(e) => return with_version(ApiError::unprocessable(e.to_string()).into_response()),
    };
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.edit_job(&job_id, &edit, n)
    })
    .await;
    match res {
        Ok(Ok(rec)) => {
            state.waiters.notify_work();
            match job_view_for(&state, &rec).await {
                Ok(view) => with_version(json_ok(StatusCode::OK, &view)),
                Err(error) => with_version(error.into_response()),
            }
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

fn artifact_name_from_str(name: &str) -> Option<ArtifactName> {
    name.parse().ok()
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
        let remaining = rec.log_len - offset;
        match tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let mut f = fs::File::open(&path)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = Vec::new();
            f.take(remaining).read_to_end(&mut buf)?;
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
    res.headers_mut().insert(
        "x-lu-state",
        header::HeaderValue::from_str(rec.state.as_str())
            .unwrap_or(header::HeaderValue::from_static("queued")),
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
        move || {
            let nodes = store.nodes(n, lease_ms)?;
            let cordons = store.cordons(None)?;
            Ok::<_, lu_store::StoreError>((nodes, cordons))
        }
    })
    .await;
    match res {
        Ok(Ok((nodes, cordons))) => {
            let stats = state
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let views = cordon_views(&cordons, n);
            with_version(json_ok(
                StatusCode::OK,
                &nodes
                    .iter()
                    .map(|node| {
                        let details = cordons
                            .iter()
                            .zip(views.iter())
                            .filter(|(cordon, _)| cordon.node == node.name)
                            .map(|(_, view)| view.clone())
                            .collect();
                        node_view(node, stats.get(&node.name), details)
                    })
                    .collect::<Vec<_>>(),
            ))
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

pub async fn get_node_stats(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let node = match NodeName::parse(&name) {
        Ok(n) => n,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || store.nodes(n, lease_ms)
    })
    .await;
    let nodes = match res {
        Ok(Ok(nodes)) => nodes,
        Ok(Err(e)) => return with_version(store_err(e).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    if !nodes.iter().any(|record| record.name == node) {
        return with_version(ApiError::not_found("not found").into_response());
    }
    let stats = state
        .stats
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&node)
        .cloned();
    match stats {
        Some(stats) => with_version(json_ok(StatusCode::OK, &stats)),
        None => with_version(json_value_ok(StatusCode::OK, json!({}))),
    }
}

pub async fn list_repos(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let repos = state.mirror.infos();
    with_version(json_value_ok(StatusCode::OK, json!({ "repos": repos })))
}

pub async fn sync_repo(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    if state.mirror.repo(&name).is_none() {
        return with_version(ApiError::not_found("not found").into_response());
    }
    let mirror = Arc::clone(&state.mirror);
    let result = tokio::task::spawn_blocking(move || mirror.sync(&name)).await;
    match result {
        Ok(Ok(sync)) => with_version(json_ok(StatusCode::OK, &sync)),
        Ok(Err(e)) => with_version(
            ApiError::new(ErrorCode::Internal, format!("repository sync failed: {e}"))
                .into_response(),
        ),
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

/// Removes provider cordons from a node. An empty body clears every cordon; `{"provider":"x"}`
/// clears just that provider. The reply lists the providers actually removed.
pub async fn uncordon_node(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
    req: axum::extract::Request,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let node = match NodeName::parse(&name) {
        Ok(n) => n,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let bytes = match read_raw_limited(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    let provider = if bytes.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return with_version(ApiError::bad_request("invalid JSON").into_response()),
        };
        match value.get("provider") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(p)) => Some(p.clone()),
            Some(_) => {
                return with_version(
                    ApiError::bad_request("provider must be a string").into_response(),
                );
            }
        }
    };
    let at = now();
    let check_node = node.clone();
    let exists = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || {
            store
                .nodes(at, lease_ms)
                .map(|nodes| nodes.iter().any(|record| record.name == check_node))
        }
    })
    .await;
    match exists {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return with_version(ApiError::not_found("not found").into_response()),
        Ok(Err(e)) => return with_version(store_err(e).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    }
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.uncordon(&node, provider.as_deref())
    })
    .await;
    match res {
        Ok(Ok(removed)) => {
            state.waiters.notify_work();
            with_version(json_ok(StatusCode::OK, &json!({ "removed": removed })))
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

#[cfg(test)]
mod tests {
    use super::*;
    use lu_proto::{JobState, DEFAULT_TIMEOUT_SECS};

    fn record(
        state: JobState,
        allow_empty: bool,
        diff_bytes: Option<u64>,
        artifacts: Vec<ArtifactName>,
    ) -> JobRecord {
        JobRecord {
            id: JobId::new(),
            state,
            spec: JobSpec {
                repo: "https://example.net/repo.git".to_owned(),
                git_ref: "main".to_owned(),
                base_commit: None,
                task: "run tests".to_owned(),
                harness: "opencode".to_owned(),
                provider: None,
                model: None,
                timeout_secs: DEFAULT_TIMEOUT_SECS,
                idle_timeout_secs: None,
                on_stuck: None,
                stuck_after_secs: None,
                idempotency_key: None,
                after: Vec::new(),
                stack_dependencies: true,
                collect: None,
                include_failed: false,
                on_conflict: Default::default(),
                allow_empty,
                requires: Vec::new(),
                tolerates: Vec::new(),
                priority: lu_proto::DEFAULT_PRIORITY,
                group: None,
                fallbacks: Vec::new(),
                expect: None,
            },
            attempt: 1,
            node: None,
            assigned_provider: None,
            assigned_model: None,
            epoch: 1,
            created_at: 1,
            updated_at: 2,
            started_at: None,
            finished_at: None,
            failure_class: None,
            summary: None,
            exit_code: Some(0),
            artifacts,
            cancel_requested: false,
            log_len: 0,
            diff_bytes,
            waiting_on: Vec::new(),
            expect_failed: false,
        }
    }

    fn diff() -> Vec<ArtifactName> {
        vec![ArtifactName::DiffPatch]
    }

    #[test]
    fn an_empty_diff_on_a_done_job_gets_the_note() {
        let r = record(JobState::Done, false, Some(0), diff());
        assert_eq!(outcome_note(&r), Some("no_changes"));
        assert_eq!(job_view(&r).outcome_note.as_deref(), Some("no_changes"));
    }

    #[test]
    fn a_nonempty_diff_has_no_note() {
        let r = record(JobState::Done, false, Some(1), diff());
        assert_eq!(outcome_note(&r), None);
        assert!(job_view(&r).outcome_note.is_none());
    }

    #[test]
    fn allow_empty_suppresses_the_note() {
        let r = record(JobState::Done, true, Some(0), diff());
        assert_eq!(outcome_note(&r), None);
        assert!(job_view(&r).outcome_note.is_none());
    }

    #[test]
    fn no_diff_artifact_and_nondone_states_have_no_note() {
        let no_diff = record(JobState::Done, false, Some(0), Vec::new());
        assert_eq!(outcome_note(&no_diff), None);
        let legacy = record(JobState::Done, false, None, diff());
        assert_eq!(outcome_note(&legacy), None);
        let running = record(JobState::Running, false, Some(0), diff());
        assert_eq!(outcome_note(&running), None);
    }

    #[test]
    fn an_expectation_failure_on_a_done_job_gets_the_note() {
        let mut r = record(JobState::Done, false, Some(120), diff());
        r.expect_failed = true;
        assert_eq!(outcome_note(&r), Some("expect_failed"));
        assert_eq!(job_view(&r).outcome_note.as_deref(), Some("expect_failed"));
    }

    #[test]
    fn an_empty_diff_takes_precedence_over_an_expectation_failure() {
        let mut r = record(JobState::Done, false, Some(0), diff());
        r.expect_failed = true;
        assert_eq!(outcome_note(&r), Some("no_changes"));
    }

    #[test]
    fn only_done_jobs_get_the_expectation_note() {
        for state in [JobState::Running, JobState::Failed, JobState::Cancelled] {
            let mut r = record(state, false, Some(120), diff());
            r.expect_failed = true;
            assert_eq!(outcome_note(&r), None, "{state:?}");
        }
    }

    #[test]
    fn every_terminal_job_view_exposes_diff_bytes() {
        for state in [JobState::Done, JobState::Failed, JobState::Cancelled] {
            let r = record(state, false, Some(4096), diff());
            let view = job_view(&r);
            assert_eq!(view.diff_bytes, Some(4096), "{state:?}");
        }
        // A terminal job without a diff reports zero, and an in-flight job reports none.
        let failed_without_diff = record(JobState::Failed, false, None, Vec::new());
        let view = job_view(&failed_without_diff);
        assert_eq!(view.diff_bytes, Some(0));
        let running = record(JobState::Running, false, Some(4096), diff());
        assert_eq!(job_view(&running).diff_bytes, None);
    }
}
