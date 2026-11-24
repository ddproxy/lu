use crate::auth::authenticate_agent;
use crate::error::ApiError;
use crate::limits::PACK_PER_TOKEN;
use crate::routes::file_body::{file_len, into_response, stream_range};
use crate::routes::{
    json_ok, read_json, read_raw_limited, store_err, with_version, MAX_DIFF_BODY, MAX_JSON_BODY,
    MAX_LOG_CHUNK_BODY, MAX_REPORT_BODY,
};
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use lu_audit::{Data, Event, Field};
use lu_proto::{
    AgentSelfView, ArtifactName, Completion, Heartbeat, JobId, JobState, NodeStats,
    WorkspaceRelease,
};
use lu_serve::PeerAddr;
use lu_staging::{pack_commit_capped, StageError};
use serde::Deserialize;
use serde_json::json;
use std::io::{Cursor, SeekFrom};
use std::sync::Arc;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

#[derive(Deserialize)]
// Unknown fields are accepted: an agent from a newer release may send more (docs/compatibility.md).
struct PollBody {
    heartbeat: Heartbeat,
    wait_secs: u64,
}

#[derive(Deserialize)]
// Unknown fields are accepted: an agent from a newer release may send more (docs/compatibility.md).
struct EpochBody {
    epoch: u64,
}

/// The self/drain/undrain routes take no input: the node comes from the agent token alone. An
/// empty body or `{}` is accepted; any field is refused.
#[derive(Deserialize)]
// Unknown fields are accepted: an agent from a newer release may send more (docs/compatibility.md).
struct EmptyBody {}

fn reject_query(uri: &Uri) -> Result<(), ApiError> {
    if uri.query().is_some() {
        return Err(ApiError::bad_request("unexpected query parameters"));
    }
    Ok(())
}

async fn read_empty_body(req: axum::extract::Request) -> Result<(), ApiError> {
    let bytes = read_raw_limited(req, MAX_JSON_BODY).await?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    serde_json::from_slice::<EmptyBody>(&bytes)
        .map_err(|_| ApiError::bad_request("invalid request body"))?;
    Ok(())
}

/// The agent's own node, bound to the bearer token; no name can be supplied by the caller.
pub async fn self_view(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(node) => node,
        Err(e) => return with_version(e.into_response()),
    };
    if let Err(e) = reject_query(&uri) {
        return with_version(e.into_response());
    }
    let at = now();
    let lease_ms = state.lease_ms;
    let store = Arc::clone(&state.store);
    let nodes = match tokio::task::spawn_blocking(move || store.nodes(at, lease_ms)).await {
        Ok(Ok(nodes)) => nodes,
        Ok(Err(e)) => return with_version(store_err(e).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let Some(record) = nodes.into_iter().find(|record| record.name == node) else {
        return with_version(ApiError::not_found("not found").into_response());
    };
    let actor = node.as_str().to_owned();
    state.record_audit(
        Event::AgentSelf,
        None,
        Some(&actor),
        &actor,
        Data::new().text(Field::Source, "agent"),
    );
    with_version(json_ok(
        StatusCode::OK,
        &AgentSelfView {
            name: record.name,
            version: record.heartbeat.version,
            draining: record.draining,
            current_job: record.current_job,
            last_heartbeat_age_secs: u64::try_from(at.wall_unix.saturating_sub(record.last_seen))
                .unwrap_or(0),
            online: record.online,
        },
    ))
}

/// Marks this agent's node as drained. Idempotent, and never takes over an operator drain.
pub async fn drain(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    uri: Uri,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(node) => node,
        Err(e) => return with_version(e.into_response()),
    };
    if let Err(e) = reject_query(&uri) {
        return with_version(e.into_response());
    }
    if let Err(e) = read_empty_body(req).await {
        return with_version(e.into_response());
    }
    let actor = node.as_str().to_owned();
    let store = Arc::clone(&state.store);
    let result = tokio::task::spawn_blocking(move || store.set_agent_drain(&node)).await;
    match result {
        Ok(Ok(())) => {
            state.record_audit(
                Event::Drain,
                None,
                Some(&actor),
                &actor,
                Data::new()
                    .bool(Field::State, true)
                    .text(Field::Source, "agent"),
            );
            state.waiters.notify_work();
            with_version(json_ok(StatusCode::OK, &json!({})))
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

/// Clears this agent's own drain. An operator drain is left in place and reported as a conflict.
pub async fn undrain(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    uri: Uri,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(node) => node,
        Err(e) => return with_version(e.into_response()),
    };
    if let Err(e) = reject_query(&uri) {
        return with_version(e.into_response());
    }
    if let Err(e) = read_empty_body(req).await {
        return with_version(e.into_response());
    }
    let actor = node.as_str().to_owned();
    let store = Arc::clone(&state.store);
    let result = tokio::task::spawn_blocking(move || store.clear_agent_drain(&node)).await;
    match result {
        Ok(Ok(_)) => {
            state.record_audit(
                Event::Drain,
                None,
                Some(&actor),
                &actor,
                Data::new()
                    .bool(Field::State, false)
                    .text(Field::Source, "agent"),
            );
            state.waiters.notify_work();
            with_version(json_ok(StatusCode::OK, &json!({})))
        }
        Ok(Err(e)) => {
            if matches!(e, lu_store::StoreError::Conflict { .. }) {
                state.record_audit(
                    Event::Drain,
                    None,
                    Some(&actor),
                    &actor,
                    Data::new()
                        .bool(Field::State, false)
                        .text(Field::Source, "agent")
                        .text(Field::Result, "conflict"),
                );
            }
            with_version(store_err(e).into_response())
        }
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

#[derive(Deserialize)]
// Unknown fields are accepted: an agent from a newer release may send more (docs/compatibility.md).
struct StagedCommitBody {
    /// Full 40-character hex object id of the commit the agent staged from.
    commit: String,
    /// Accepted for clients that put the lease epoch in the body instead of the query.
    epoch: Option<u64>,
}

#[derive(Deserialize)]
// Unknown fields are accepted: an agent from a newer release may send more (docs/compatibility.md).
struct MergeBaseBody {
    /// Two or more full 40-character hex object ids.
    commits: Vec<String>,
    /// Accepted for clients that put the lease epoch in the body instead of the query.
    epoch: Option<u64>,
}

enum MergeBaseError {
    NoAncestor,
    Unavailable,
    Git,
}

fn valid_commit_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Normalises a commit id to the lowercase form the store records.
fn commit_id(value: &str) -> Option<String> {
    valid_commit_id(value).then(|| value.to_ascii_lowercase())
}

enum BasePackError {
    Provider(String),
    Pack(StageError),
}

// query params parsed manually from URI to avoid axum "query" feature dependency

pub async fn poll(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let body: PollBody = match read_json(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    if body.wait_secs > 30 {
        return with_version(ApiError::bad_request("wait_secs must be 0 to 30").into_response());
    }
    if body.heartbeat.node != node {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    // The latest stats replace the previous ones even when both fields are absent: a heartbeat
    // from an older agent, or an idle node, then leaves no stale job tree behind.
    let stats = NodeStats {
        host: body.heartbeat.host.clone(),
        jobs: body.heartbeat.jobs.clone(),
        labels: body.heartbeat.labels.clone(),
        label_sources: body.heartbeat.label_sources.clone(),
        workspace_bytes: body.heartbeat.workspace_bytes,
        workspace_count: body.heartbeat.workspace_count,
        workspace_reclaimed_bytes_total: body.heartbeat.workspace_reclaimed_bytes_total,
        workspace_evictions_total: body.heartbeat.workspace_evictions_total,
        workspace_delete_errors_total: body.heartbeat.workspace_delete_errors_total,
        workspaces: body.heartbeat.workspaces.clone(),
    };
    state
        .stats
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(node.clone(), stats);
    let wait_secs = body.wait_secs;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    let permit = if wait_secs > 0 {
        state.waiters.try_acquire_poll()
    } else {
        None
    };
    loop {
        let n = now();
        // heartbeat
        let hb_res = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            let hb = body.heartbeat.clone();
            move || store.heartbeat(&hb, n)
        })
        .await;
        if hb_res.is_err() {
            return with_version(ApiError::internal().into_response());
        }
        if let Ok(Err(e)) = hb_res {
            return with_version(store_err(e).into_response());
        }

        // collect events: cancels and maybe assignment
        let cancels = {
            let store = Arc::clone(&state.store);
            let node_c = node.clone();
            match tokio::task::spawn_blocking(move || store.pending_cancels(&node_c)).await {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return with_version(store_err(e).into_response()),
                Err(_) => return with_version(ApiError::internal().into_response()),
            }
        };
        let mut events: Vec<serde_json::Value> = cancels
            .into_iter()
            .map(|(job_id, epoch)| json!({"kind":"cancel","job_id": job_id.to_string(), "epoch": epoch}))
            .collect();
        if let Some(clean) = state
            .pending_clean
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&node)
        {
            events.extend(clean);
        }

        // try assignment only if no current work implied? store enforces no job held.
        let assignment = {
            let store = Arc::clone(&state.store);
            let node_c = node.clone();
            let lease_ms = state.lease_ms;
            let n2 = now();
            let schedule = state.schedule();
            let limits = state.dispatch_limits();
            match tokio::task::spawn_blocking(move || match &schedule {
                Some(schedule) => store
                    .next_assignment_scheduled_with_limits(&node_c, n2, lease_ms, schedule, limits),
                None => store.next_assignment_with_limits(&node_c, n2, lease_ms, limits),
            })
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return with_version(store_err(e).into_response()),
                Err(_) => return with_version(ApiError::internal().into_response()),
            }
        };
        if let Some(a) = assignment {
            state.record_audit(
                Event::Leased,
                Some(a.job_id.as_str()),
                Some(node.as_str()),
                node.as_str(),
                Data::new()
                    .unsigned(Field::Epoch, a.epoch)
                    .text(Field::Provider, a.provider.as_deref().unwrap_or("any"))
                    .text(Field::Model, a.model.as_deref().unwrap_or("")),
            );
            let mut event = json!({
                "kind":"assign",
                "job_id": a.job_id.to_string(),
                "epoch": a.epoch,
                "spec": a.spec,
            });
            if let Some(provider) = a.provider {
                event["provider"] = provider.into();
            }
            if let Some(model) = a.model {
                event["model"] = model.into();
            }
            events.push(event);
        }

        if !events.is_empty() {
            // Poll implies renewal for every job this node holds, not just jobs with a pending
            // cancel. A job that missed its renew still has a live lease while the node polls.
            let held = {
                let store = Arc::clone(&state.store);
                let node_c = node.clone();
                match tokio::task::spawn_blocking(move || store.held_jobs(&node_c)).await {
                    Ok(Ok(held)) => held,
                    Ok(Err(e)) => return with_version(store_err(e).into_response()),
                    Err(_) => return with_version(ApiError::internal().into_response()),
                }
            };
            for (jid, epoch) in held {
                let store = Arc::clone(&state.store);
                let lease_ms = state.lease_ms;
                let n3 = now();
                let _ = tokio::task::spawn_blocking(move || {
                    store.renew_lease(&jid, epoch, n3, lease_ms)
                })
                .await;
            }
            return with_version(json_ok(StatusCode::OK, &events));
        }

        if wait_secs == 0 || state.waiters.is_shutting_down() {
            return with_version(StatusCode::NO_CONTENT.into_response());
        }
        let Some(_p) = &permit else {
            return with_version(StatusCode::NO_CONTENT.into_response());
        };
        let nowi = tokio::time::Instant::now();
        if nowi >= deadline {
            return with_version(StatusCode::NO_CONTENT.into_response());
        }
        let remaining = deadline.saturating_duration_since(nowi);
        let _woke = state
            .waiters
            .wait_work(remaining.min(std::time::Duration::from_secs(wait_secs)))
            .await;
        if state.waiters.is_shutting_down() || tokio::time::Instant::now() >= deadline {
            return with_version(StatusCode::NO_CONTENT.into_response());
        }
    }
}

pub async fn start_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let body: EpochBody = match read_json(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    // ensure job is bound to this node? store.start_job validates epoch/state; but it doesn't check node ownership explicitly in trait.
    // We must enforce "agent token for agent routes..." and node binding; start implies the agent holds the lease - but trait doesn't take node.
    // Verify via get_job that node matches.
    let rec_check = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if rec_check.node.as_ref() != Some(&node) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let n = now();
    // renew implied by start: extend lease as well? start moves to running; renew endpoint extends lease.
    // store.start_job may update lease timing; spec says "Also implied by `poll`, `log` and `start`" for renew.
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        move || store.start_job(&jid, body.epoch, n)
    })
    .await;
    match res {
        Ok(Ok(rec)) => {
            // best-effort renew
            let _ = tokio::task::spawn_blocking({
                let store = Arc::clone(&state.store);
                let jid = job_id.clone();
                let lease_ms = state.lease_ms;
                let n2 = now();
                move || store.renew_lease(&jid, body.epoch, n2, lease_ms)
            })
            .await;
            state.record_audit(
                Event::Started,
                Some(rec.id.as_str()),
                Some(node.as_str()),
                node.as_str(),
                Data::new()
                    .unsigned(Field::Attempt, u64::from(rec.attempt))
                    .unsigned(Field::Epoch, body.epoch),
            );
            state.waiters.notify_job(rec.id.as_ref());
            state.waiters.notify_work();
            with_version(json_ok(
                StatusCode::OK,
                &crate::routes::operator::job_view_with_ttl(&rec, state.blocked_dependent_ttl_secs),
            ))
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

pub async fn renew_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let body: EpochBody = match read_json(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    let rec_check = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if rec_check.node.as_ref() != Some(&node) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        let lease_ms = state.lease_ms;
        move || store.renew_lease(&jid, body.epoch, n, lease_ms)
    })
    .await;
    match res {
        Ok(Ok(cancel)) => with_version(json_ok(StatusCode::OK, &json!({"cancel": cancel}))),
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

async fn write_log_chunk(path: &std::path::Path, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        // Private from the first byte, not narrowed after creation.
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .await?;
        let meta = tokio::fs::symlink_metadata(parent).await?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "artifact job directory is not a real directory",
            ));
        }
    }
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true).mode(0o600);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW);
    let mut f = opts.open(path).await?;
    // A partial write that failed may have left bytes past the stored length. Trim them so a
    // retry lands exactly at the reserved offset.
    if f.metadata().await?.len() > offset {
        f.set_len(offset).await?;
    }
    f.seek(SeekFrom::Start(offset)).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    Ok(())
}

pub async fn append_log(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    // check ownership
    let rec_check = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if rec_check.node.as_ref() != Some(&node) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let q = crate::routes::parse_query(&uri);
    let Some(epoch) = q.get("epoch").and_then(|s| s.parse::<u64>().ok()) else {
        return with_version(ApiError::bad_request("missing or invalid epoch").into_response());
    };
    let Some(offset) = q.get("offset").and_then(|s| s.parse::<u64>().ok()) else {
        return with_version(ApiError::bad_request("missing or invalid offset").into_response());
    };
    let bytes = match read_raw_limited(req, MAX_LOG_CHUNK_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    // Logs are scrubbed before they reach disk. The stored length therefore differs from the
    // sender's byte count; the response reports the new length so the uploader can resume there.
    let scrubbed = state.scrubber.scrub(&bytes, false);
    let bytes = scrubbed.data;
    let nbytes = bytes.len() as u64;
    // Data may fill the log up to the cap minus the marker, so any log that is cut ends with the
    // marker and never exceeds the cap; once the marker is in, later chunks are dropped with 200.
    const MARKER: &[u8] = b"\n[log truncated]\n";
    let cap = lu_proto::MAX_LOG_BYTES;
    let limit = cap - MARKER.len() as u64;
    let current_len = rec_check.log_len;
    let (to_write, marker_only) = if current_len >= cap {
        (Vec::new(), true)
    } else if current_len + nbytes > limit {
        let allowed = limit.saturating_sub(current_len) as usize;
        let mut v = bytes[..allowed.min(bytes.len())].to_vec();
        v.extend_from_slice(MARKER);
        (v, true)
    } else {
        (bytes.to_vec(), false)
    };
    let actual_append = to_write.len() as u64;
    let mut stored_len = current_len;
    if actual_append > 0 {
        let res = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            let jid = job_id.clone();
            move || store.reserve_log(&jid, epoch, offset, actual_append)
        })
        .await;
        let new_len = match res {
            Ok(Ok(new_len)) => new_len,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        };
        stored_len = new_len;
        let path = state.artifacts_dir.join(job_id.to_string()).join("run.log");
        if let Err(error) = write_log_chunk(&path, offset, &to_write).await {
            tracing::warn!(%error, job = %job_id, "log write failed; releasing the reservation");
            let rollback = tokio::task::spawn_blocking({
                let store = Arc::clone(&state.store);
                let jid = job_id.clone();
                move || store.unreserve_log(&jid, epoch, new_len, actual_append)
            })
            .await;
            match rollback {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(?e, job = %job_id, "could not release the log reservation")
                }
                Err(e) => {
                    tracing::warn!(%e, job = %job_id, "log rollback task failed")
                }
            }
            return with_version(ApiError::internal().into_response());
        }
    } else if marker_only {
        // If cap already reached or we dropped everything, we still must validate offset/epoch via reserve_log semantics?
        // "offset must equal the stored length..." - try reserving 0 bytes? store may not allow 0. Safer: just check via get_job and compare.
        // But we must also honor epoch fencing for writes. Use reserve_log with n=0 if possible? trait takes u64 n.
        let res = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            let jid = job_id.clone();
            move || store.reserve_log(&jid, epoch, offset, 0)
        })
        .await;
        match res {
            Ok(Ok(new_len)) => stored_len = new_len,
            Ok(Err(lu_store::StoreError::Conflict { length })) => {
                // if offset mismatch conflict, report; but some impls might reject n=0 differently.
                return with_version(
                    ApiError::conflict("conflict")
                        .with_log_length(length.unwrap_or(rec_check.log_len))
                        .into_response(),
                );
            }
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    }
    // renew implied by log
    let _ = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        let lease_ms = state.lease_ms;
        let n = now();
        move || store.renew_lease(&jid, epoch, n, lease_ms)
    })
    .await;
    if scrubbed.hits > 0 {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        let hits = scrubbed.hits;
        match tokio::task::spawn_blocking(move || store.record_scrub(&jid, epoch, hits, false))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(?error, job = %job_id, "could not record log scrub count")
            }
            Err(error) => {
                tracing::warn!(%error, job = %job_id, "log scrub count task failed")
            }
        }
    }
    state.waiters.notify_job(job_id.as_ref());
    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&stored_len.to_string()) {
        response.headers_mut().insert("x-lu-log-length", value);
    }
    with_version(response)
}

fn artifact_name_from_str(name: &str) -> Option<ArtifactName> {
    name.parse().ok()
}

async fn put_artifact_impl(
    state: Arc<AppState>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    id: String,
    name: &str,
    uri: axum::http::Uri,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let Some(art) = artifact_name_from_str(name) else {
        return with_version(ApiError::not_found("not found").into_response());
    };
    // agent may store diff.patch or report.json per contract
    if !matches!(art, ArtifactName::DiffPatch | ArtifactName::ReportJson) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let rec_check = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if rec_check.node.as_ref() != Some(&node) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    if !matches!(
        rec_check.state,
        JobState::Leased | JobState::Running | JobState::Recovering
    ) {
        return with_version(ApiError::conflict("job is not active").into_response());
    }
    let cap = art.max_bytes();
    let body_limit = match art {
        ArtifactName::DiffPatch => MAX_DIFF_BODY,
        ArtifactName::ReportJson => MAX_REPORT_BODY,
        _ => MAX_REPORT_BODY,
    };
    let q = crate::routes::parse_query(&uri);
    let Some(epoch) = q.get("epoch").and_then(|s| s.parse::<u64>().ok()) else {
        return with_version(ApiError::bad_request("missing or invalid epoch").into_response());
    };
    // A stale holder must not reach the filesystem: record_artifact would reject the epoch,
    // but only after the newer attempt's file had been replaced.
    if rec_check.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    let bytes = match read_raw_limited(req, body_limit).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    let scrubbed = state.scrubber.scrub(&bytes, art == ArtifactName::DiffPatch);
    let bytes = scrubbed.data;
    let nbytes = bytes.len() as u64;
    // Record first: the store checks the epoch inside its transaction, so a holder that lost its
    // lease writes nothing. A failed write after this leaves the record without a file, which a
    // fetch treats as missing and the agent's retry replaces.
    let rec_res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        move || store.record_artifact(&jid, epoch, art, nbytes)
    })
    .await;
    match rec_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return with_version(store_err(e).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    }
    // write via ArtifactDir (enforces cap, atomic rename, modes). ensure job dir exists.
    let ad = match state.artifact_dir() {
        Ok(a) => a,
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let job_id_s = job_id.to_string();
    // create dir if missing
    match ad.create(&job_id_s) {
        Ok(_) => {}
        Err(lu_staging::ArtifactError::AlreadyExists(_)) => {}
        Err(_) => return with_version(ApiError::internal().into_response()),
    }
    // The store's epoch check above cannot cover a lease transfer that happens while we are
    // writing. Recheck immediately before the atomic rename so a holder that lost the job
    // between `record_artifact` and `write` cannot overwrite the newer attempt's artifact.
    let rec_final = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if rec_final.node.as_ref() != Some(&node) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    if rec_final.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    let res_write = tokio::task::spawn_blocking({
        let ad = ad.clone();
        let jid = job_id_s.clone();
        move || {
            ad.write(
                &jid,
                match art {
                    ArtifactName::DiffPatch => lu_staging::ArtifactFile::DiffPatch,
                    ArtifactName::RunLog => lu_staging::ArtifactFile::RunLog,
                    ArtifactName::ReportJson => lu_staging::ArtifactFile::ReportJson,
                },
                Cursor::new(bytes.to_vec()),
                cap,
            )
        }
    })
    .await;
    match res_write {
        Ok(Ok(_)) => {}
        Ok(Err(lu_staging::ArtifactError::TooLarge)) => {
            return with_version(ApiError::too_large("artifact too large").into_response())
        }
        Ok(Err(_)) => return with_version(ApiError::internal().into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    }
    if scrubbed.hits > 0 {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        let hits = scrubbed.hits;
        let patch_scrubbed = scrubbed.patch_scrubbed;
        match tokio::task::spawn_blocking(move || {
            store.record_scrub(&jid, epoch, hits, patch_scrubbed)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(?error, job = %job_id, "could not record artifact scrub count")
            }
            Err(error) => {
                tracing::warn!(%error, job = %job_id, "artifact scrub count task failed")
            }
        }
    }
    if art == ArtifactName::DiffPatch && scrubbed.patch_secret_hits.count > 0 {
        let hits_view = lu_proto::view::PatchSecretHitsView {
            count: scrubbed.patch_secret_hits.count,
            rules: scrubbed.patch_secret_hits.rules.clone(),
        };
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        let hits_view_for_store = hits_view.clone();
        match tokio::task::spawn_blocking(move || {
            store.record_patch_secret_hits(&jid, epoch, &hits_view_for_store)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(?error, job = %job_id, "could not record patch secret hits")
            }
            Err(error) => {
                tracing::warn!(%error, job = %job_id, "patch secret hits task failed")
            }
        }
        state.record_audit(
            Event::PatchSecretDetected,
            Some(job_id.as_str()),
            Some(node.as_str()),
            node.as_str(),
            Data::new()
                .unsigned(Field::Count, hits_view.count)
                .text(Field::Rules, &hits_view.rules.join("+")),
        );
    }
    state.waiters.notify_job(job_id.as_ref());
    with_version(StatusCode::OK.into_response())
}

pub async fn complete_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let completion: Completion = match read_json(req, MAX_JSON_BODY).await {
        Ok(c) => c,
        Err(e) => return with_version(e.into_response()),
    };
    if completion.job_id != job_id {
        return with_version(ApiError::bad_request("job id mismatch").into_response());
    }
    let rec_check = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if rec_check.node.as_ref() != Some(&node) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let stuck_mode = completion.stuck_mode;
    let failure = match completion.outcome {
        lu_proto::Outcome::Done | lu_proto::Outcome::Cancelled => None,
        lu_proto::Outcome::Failed(class) => {
            // A fallback attempt must cordon the pair it actually ran on.
            let provider = rec_check.assigned_provider.clone();
            Some((class, provider))
        }
    };
    // Mirrors the store's condition for inserting a provider cordon in `complete`.
    let cordon_provider = failure
        .as_ref()
        .and_then(|(class, provider)| {
            (*class == lu_proto::FailureClass::Provider).then(|| provider.clone())?
        })
        .filter(|_| {
            lu_proto::FailureClass::Provider.retryable(rec_check.attempt, state.max_retries)
        });
    let cordon_ms = cordon_ms_for(&completion, state.cordon_ms);
    let retry_after_secs = completion.retry_after_secs;
    let exit_code = completion.exit_code;
    let completion_outcome = completion.outcome;
    let epoch = completion.epoch;
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let max_retries = state.max_retries;
        move || store.complete(&completion, failure, n, max_retries, cordon_ms)
    })
    .await;
    match res {
        Ok(Ok(rec)) => {
            match crate::routes::operator::outcome_note(&rec) {
                Some("no_changes") => state.count_no_changes(),
                Some("expect_failed") => state.count_expect_failed(),
                _ => {}
            }
            state.count_stuck_stopped(stuck_mode);
            let (event, outcome) = match rec.state {
                lu_proto::JobState::Done => (Event::Finished, "done"),
                lu_proto::JobState::Failed => (Event::Failed, "failed"),
                lu_proto::JobState::Queued => (Event::Retried, "retried"),
                lu_proto::JobState::Cancelled => (Event::Cancelled, "cancelled"),
                _ => (Event::Failed, "unknown"),
            };
            let mut data = Data::new()
                .unsigned(Field::Attempt, u64::from(rec.attempt))
                .unsigned(Field::Epoch, epoch)
                .text(Field::Outcome, outcome)
                .text(Field::State, rec.state.as_str())
                .text(Field::ActorKind, "agent");
            if let lu_proto::Outcome::Failed(class) = completion_outcome {
                data = data.text(Field::Class, class.as_str());
            }
            if let Some(code) = exit_code {
                data = data.signed(Field::ExitCode, i64::from(code));
            }
            if let Some(secs) = retry_after_secs {
                data = data.unsigned(Field::RetryAfterSecs, secs);
            }
            state.record_audit(
                event,
                Some(rec.id.as_str()),
                Some(node.as_str()),
                node.as_str(),
                data,
            );
            if let Some(provider) = cordon_provider.as_deref() {
                state.record_audit(
                    Event::CordonSet,
                    Some(rec.id.as_str()),
                    Some(node.as_str()),
                    node.as_str(),
                    Data::new()
                        .text(Field::Provider, provider)
                        .text(Field::Class, "provider")
                        .text(
                            Field::Source,
                            if retry_after_secs.is_some() {
                                "reiki"
                            } else {
                                "failure"
                            },
                        )
                        .unsigned(Field::CordonMs, cordon_ms),
                );
            }
            state.waiters.notify_job(rec.id.as_ref());
            state.waiters.notify_work();
            with_version(json_ok(
                StatusCode::OK,
                &crate::routes::operator::job_view_with_ttl(&rec, state.blocked_dependent_ttl_secs),
            ))
        }
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

async fn owned_job(
    state: &Arc<AppState>,
    node: &lu_proto::NodeName,
    job_id: &JobId,
) -> Result<lu_store::JobRecord, Box<Response>> {
    let jid = job_id.clone();
    let store = Arc::clone(&state.store);
    let rec = match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
        Ok(Ok(rec)) => rec,
        Ok(Err(e)) => return Err(Box::new(with_version(store_err(e).into_response()))),
        Err(_) => return Err(Box::new(with_version(ApiError::internal().into_response()))),
    };
    if rec.node.as_ref() != Some(node) {
        return Err(Box::new(with_version(
            ApiError::forbidden("forbidden").into_response(),
        )));
    }
    Ok(rec)
}

fn epoch_from_uri(uri: &axum::http::Uri) -> Result<u64, Box<Response>> {
    let q = crate::routes::parse_query(uri);
    q.get("epoch")
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| {
            Box::new(with_version(
                ApiError::bad_request("missing or invalid epoch").into_response(),
            ))
        })
}

fn dependency_diff_path(state: &AppState, dep: &JobId) -> std::path::PathBuf {
    state.artifacts_dir.join(dep.to_string()).join("diff.patch")
}

/// Confirms that a finished job's artifacts are stored and that no queued or running job still
/// stacks on or collects from its workspace. The agent uses this instead of keeping stacking
/// workspaces indefinitely.
pub async fn workspace_release(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let rec = match owned_job(&state, &node, &job_id).await {
        Ok(rec) => rec,
        Err(res) => return *res,
    };
    let epoch = match epoch_from_uri(&uri) {
        Ok(epoch) => epoch,
        Err(res) => return *res,
    };
    if rec.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    let patch_stored = rec.artifacts.contains(&ArtifactName::DiffPatch)
        || rec.diff_bytes.is_none()
        || rec.diff_bytes == Some(0);
    let report_stored = rec.artifacts.contains(&ArtifactName::ReportJson);
    let active = {
        let store = Arc::clone(&state.store);
        match tokio::task::spawn_blocking(move || store.list_jobs(None, 10_000, None)).await {
            Ok(Ok(jobs)) => jobs,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    }
    .into_iter()
    .any(|job| {
        matches!(
            job.state,
            JobState::Blocked
                | JobState::Queued
                | JobState::Leased
                | JobState::Running
                | JobState::Recovering
                | JobState::NeedsInput
        ) && job.spec.after.contains(&job_id)
    });
    let (can_delete, reason) = if !report_stored || !patch_stored {
        (false, "final artifacts are not stored yet")
    } else if active {
        (false, "dependants still need the workspace")
    } else {
        (true, "released")
    };
    with_version(json_ok(
        StatusCode::OK,
        &WorkspaceRelease {
            can_delete,
            reason: reason.to_owned(),
        },
    ))
}

pub async fn list_deps(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let rec = match owned_job(&state, &node, &job_id).await {
        Ok(rec) => rec,
        Err(res) => return *res,
    };
    let epoch = match epoch_from_uri(&uri) {
        Ok(epoch) => epoch,
        Err(res) => return *res,
    };
    if rec.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    let mut deps = Vec::with_capacity(rec.spec.after.len());
    for dep in &rec.spec.after {
        let jid = dep.clone();
        let store = Arc::clone(&state.store);
        let dep_rec = match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(rec)) => rec,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        };
        let diff_bytes = tokio::fs::metadata(dependency_diff_path(&state, dep))
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        deps.push(json!({
            "job_id": dep.to_string(),
            "state": crate::routes::job_state_str(dep_rec.state),
            "diff_bytes": diff_bytes,
            "idempotency_key": dep_rec.spec.idempotency_key.clone(),
            "provider": dep_rec.assigned_provider.clone().or_else(|| dep_rec.spec.provider.clone()),
            "model": dep_rec.assigned_model.clone().or_else(|| dep_rec.spec.model.clone()),
            "node": dep_rec.node.as_ref().map(|node| node.to_string()),
            "started_at": dep_rec.started_at,
            "finished_at": dep_rec.finished_at,
            "failure_class": dep_rec.failure_class.map(crate::routes::failure_class_str),
            "summary": dep_rec.summary,
            "staged_commit": dep_rec.staged_commit,
        }));
    }
    with_version(json_ok(StatusCode::OK, &deps))
}

/// Records the commit an agent staged before dependency patches were applied. The lease epoch
/// fences the write, so a transferred lease cannot overwrite a newer attempt's base.
pub async fn set_staged_commit(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let body: StagedCommitBody = match read_json(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    let Some(commit) = commit_id(&body.commit) else {
        return with_version(ApiError::bad_request("invalid commit").into_response());
    };
    let rec = match owned_job(&state, &node, &job_id).await {
        Ok(rec) => rec,
        Err(res) => return *res,
    };
    let epoch = match epoch_from_uri(&uri) {
        Ok(epoch) => epoch,
        Err(_) => match body.epoch {
            Some(epoch) => epoch,
            None => {
                return with_version(
                    ApiError::bad_request("missing or invalid epoch").into_response(),
                )
            }
        },
    };
    if rec.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    let jid = job_id.clone();
    let stored = commit.clone();
    let store = Arc::clone(&state.store);
    let result = tokio::task::spawn_blocking(move || {
        store.record_staged_commit(&jid, epoch, &stored, now())
    })
    .await;
    match result {
        Ok(Ok(rec)) => with_version(json_ok(
            StatusCode::OK,
            &crate::routes::operator::job_view_with_ttl(&rec, state.blocked_dependent_ttl_secs),
        )),
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

/// Resolves the common ancestor of two or more dependency staged commits. Disagreeing dependency
/// bases are read-only git work, so the result is computed on the blocking pool and fenced by the
/// job's lease epoch.
pub async fn merge_base(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
    req: axum::extract::Request,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let body: MergeBaseBody = match read_json(req, MAX_JSON_BODY).await {
        Ok(b) => b,
        Err(e) => return with_version(e.into_response()),
    };
    if body.commits.len() < 2 || body.commits.len() > lu_proto::MAX_AFTER_JOBS {
        return with_version(ApiError::bad_request("invalid commit list").into_response());
    }
    let Some(commits) = body
        .commits
        .iter()
        .map(|value| commit_id(value))
        .collect::<Option<Vec<_>>>()
    else {
        return with_version(ApiError::bad_request("invalid commit").into_response());
    };
    let rec = match owned_job(&state, &node, &job_id).await {
        Ok(rec) => rec,
        Err(res) => return *res,
    };
    let epoch = match epoch_from_uri(&uri) {
        Ok(epoch) => epoch,
        Err(_) => match body.epoch {
            Some(epoch) => epoch,
            None => {
                return with_version(
                    ApiError::bad_request("missing or invalid epoch").into_response(),
                )
            }
        },
    };
    if rec.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    let repo = rec.spec.repo.clone();
    let resolved = tokio::task::spawn_blocking(move || -> Result<String, MergeBaseError> {
        for provider in &state.repo_providers {
            let repository = match provider.open(&repo) {
                Ok(Some(repository)) => repository,
                Ok(None) => continue,
                Err(_) => return Err(MergeBaseError::Git),
            };
            let mut base = git2::Oid::from_str(&commits[0]).map_err(|_| MergeBaseError::Git)?;
            for value in &commits[1..] {
                let oid = git2::Oid::from_str(value).map_err(|_| MergeBaseError::Git)?;
                base = repository
                    .merge_base(base, oid)
                    .map_err(|_| MergeBaseError::NoAncestor)?;
            }
            return Ok(base.to_string());
        }
        Err(MergeBaseError::Unavailable)
    })
    .await;
    match resolved {
        Ok(Ok(commit)) => with_version(json_ok(
            StatusCode::OK,
            &serde_json::json!({ "commit": commit }),
        )),
        Ok(Err(MergeBaseError::NoAncestor)) => {
            with_version(ApiError::conflict("no common merge base").into_response())
        }
        Ok(Err(MergeBaseError::Unavailable)) => {
            with_version(ApiError::not_found("base not available").into_response())
        }
        Ok(Err(MergeBaseError::Git)) => with_version(ApiError::internal().into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

pub async fn get_dep_diff(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path((id, dep)): Path<(String, String)>,
    uri: axum::http::Uri,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let dep_id = match JobId::parse(&dep) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let rec = match owned_job(&state, &node, &job_id).await {
        Ok(rec) => rec,
        Err(res) => return *res,
    };
    let epoch = match epoch_from_uri(&uri) {
        Ok(epoch) => epoch,
        Err(res) => return *res,
    };
    if rec.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    if !rec.spec.after.contains(&dep_id) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let path = dependency_diff_path(&state, &dep_id);
    let cap = ArtifactName::DiffPatch.max_bytes();
    let result = async {
        let len = file_len(&path, cap).await?;
        stream_range(&path, cap, 0, len).await
    }
    .await;
    match result {
        Ok(file) => with_version(into_response(
            file,
            "application/octet-stream",
            ArtifactName::DiffPatch.file_name(),
        )),
        Err(error) => with_version(error.into_response()),
    }
}

/// Serves one fixed artifact of a dependency that is in this job's `after` list. The artifact
/// name comes from an allow-list, not from untrusted dependency text, so it cannot become a path.
pub async fn get_dep_artifact(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path((id, dep, name)): Path<(String, String, String)>,
    uri: axum::http::Uri,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let artifact = match name.as_str() {
        "diff.patch" => ArtifactName::DiffPatch,
        "report.json" => ArtifactName::ReportJson,
        "run.log" => ArtifactName::RunLog,
        _ => return with_version(ApiError::not_found("not found").into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let dep_id = match JobId::parse(&dep) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let rec = match owned_job(&state, &node, &job_id).await {
        Ok(rec) => rec,
        Err(res) => return *res,
    };
    let epoch = match epoch_from_uri(&uri) {
        Ok(epoch) => epoch,
        Err(res) => return *res,
    };
    if rec.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    if !rec.spec.after.contains(&dep_id) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let query = crate::routes::parse_query(&uri);
    let tail = match query.get("tail") {
        Some(text) => match text.parse::<u64>().ok() {
            Some(tail) if tail > 0 => Some(tail),
            _ => return with_version(ApiError::bad_request("invalid tail").into_response()),
        },
        None => None,
    };
    let path = state
        .artifacts_dir
        .join(dep_id.to_string())
        .join(artifact.file_name());
    let cap = artifact.max_bytes();
    let result = async {
        let len = file_len(&path, cap).await?;
        let count = tail.map(|tail| tail.min(len)).unwrap_or(len);
        // Seek straight to the tail; never read (or hold) the prefix in memory.
        stream_range(&path, cap, len - count, count).await
    }
    .await;
    match result {
        Ok(file) => with_version(into_response(
            file,
            "application/octet-stream",
            artifact.file_name(),
        )),
        Err(error) => with_version(error.into_response()),
    }
}

pub async fn put_diff_artifact(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
    req: axum::extract::Request,
) -> Response {
    put_artifact_impl(state, peer, headers, id, "diff.patch", uri, req).await
}

pub async fn put_report_artifact(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
    req: axum::extract::Request,
) -> Response {
    put_artifact_impl(state, peer, headers, id, "report.json", uri, req).await
}

/// Serves a job's base commit as a git pack. The commit is fenced by the job's node and epoch.
pub async fn job_base(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(n) => n,
        Err(e) => return with_version(e.into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let rec = {
        let store = Arc::clone(&state.store);
        let jid = job_id.clone();
        match tokio::task::spawn_blocking(move || store.get_job(&jid)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    };
    if rec.node.as_ref() != Some(&node) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let q = crate::routes::parse_query(&uri);
    let Some(epoch) = q.get("epoch").and_then(|s| s.parse::<u64>().ok()) else {
        return with_version(ApiError::bad_request("missing or invalid epoch").into_response());
    };
    if rec.epoch != epoch {
        return with_version(ApiError::stale_epoch().into_response());
    }
    let Some(commit_id) = rec.spec.base_commit.as_deref() else {
        return with_version(ApiError::not_found("no base commit").into_response());
    };
    let Ok(oid) = git2::Oid::from_str(commit_id) else {
        return with_version(ApiError::not_found("invalid base commit").into_response());
    };
    // Packing walks trees and copies objects. Only a few may run at once, and each node has a
    // small budget, so repeated requests cannot monopolise the controller's CPU and disk.
    let Ok(slot) = Arc::clone(&state.pack_slots).try_acquire_owned() else {
        return with_version(ApiError::rate_limited(1).into_response());
    };
    if let Err(retry_after) = state.pack_limiter.allow(node.as_ref(), PACK_PER_TOKEN) {
        drop(slot);
        return with_version(ApiError::rate_limited(retry_after).into_response());
    }

    let packed = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let repo = rec.spec.repo.clone();
        let cap = state.max_pack_bytes;
        move || -> Result<Option<Vec<u8>>, BasePackError> {
            for provider in &state.repo_providers {
                let repository = provider.open(&repo).map_err(BasePackError::Provider)?;
                if let Some(repository) = repository {
                    let bytes =
                        pack_commit_capped(&repository, oid, cap).map_err(BasePackError::Pack)?;
                    return Ok(Some(bytes));
                }
            }
            Ok(None)
        }
    })
    .await;

    match packed {
        Ok(Ok(Some(pack))) => with_version(
            (
                [
                    (header::CONTENT_TYPE, "application/x-git-pack"),
                    (
                        header::HeaderName::from_static("x-lu-base-commit"),
                        commit_id,
                    ),
                ],
                pack,
            )
                .into_response(),
        ),
        Ok(Ok(None)) => with_version(ApiError::not_found("base not available").into_response()),
        Ok(Err(BasePackError::Provider(message))) => {
            tracing::warn!(%message, "base provider failed");
            with_version(ApiError::internal().into_response())
        }
        Ok(Err(BasePackError::Pack(StageError::PackTooLarge { .. }))) => {
            with_version(ApiError::too_large("base pack too large").into_response())
        }
        Ok(Err(BasePackError::Pack(error))) => {
            tracing::warn!(%error, "could not pack base commit");
            with_version(ApiError::internal().into_response())
        }
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

/// The agent's estimate of when the provider returns, kept within sane bounds, replaces the
/// configured cordon length for a provider failure.
fn cordon_ms_for(completion: &lu_proto::Completion, default_ms: u64) -> u64 {
    const MIN_MS: u64 = 5_000;
    const MAX_MS: u64 = 24 * 60 * 60 * 1_000;
    match (completion.outcome, completion.retry_after_secs) {
        (lu_proto::Outcome::Failed(lu_proto::FailureClass::Provider), Some(secs)) => {
            secs.saturating_mul(1_000).clamp(MIN_MS, MAX_MS)
        }
        _ => default_ms,
    }
}

#[cfg(test)]
mod cordon_tests {
    use super::*;
    use lu_proto::{Completion, FailureClass, Outcome};

    fn completion(outcome: Outcome, retry: Option<u64>) -> Completion {
        Completion {
            job_id: JobId::new(),
            epoch: 1,
            outcome,
            exit_code: None,
            summary: None,
            retry_after_secs: retry,
            stuck_mode: None,
            expect_failed: false,
        }
    }

    #[test]
    fn a_provider_failure_cordons_for_the_reported_time_within_bounds() {
        let p = Outcome::Failed(FailureClass::Provider);
        assert_eq!(cordon_ms_for(&completion(p, Some(150)), 1), 150_000);
        assert_eq!(cordon_ms_for(&completion(p, Some(0)), 1), 5_000);
        assert_eq!(cordon_ms_for(&completion(p, Some(u64::MAX)), 1), 86_400_000);
        assert_eq!(cordon_ms_for(&completion(p, None), 42), 42);
    }

    #[test]
    fn other_outcomes_keep_the_configured_length() {
        let e = Outcome::Failed(FailureClass::Execution);
        assert_eq!(cordon_ms_for(&completion(e, Some(150)), 42), 42);
        assert_eq!(cordon_ms_for(&completion(Outcome::Done, Some(150)), 42), 42);
    }
}

#[cfg(test)]
mod poll_renewal_tests {
    use super::*;
    use axum::extract::State;
    use lu_auth::sha256_hex;
    use lu_config::{ControllerAgent, ControllerConfig, ControllerOperator};
    use lu_proto::{
        ArtifactName, Completion, Heartbeat, JobSpec, JobState, NodeName, DEFAULT_PRIORITY,
        DEFAULT_TIMEOUT_SECS,
    };
    use lu_store::{JobCounts, JobRecord, Now, Store, StoreError};
    use std::net::SocketAddr;
    use std::sync::Mutex;

    const TOKEN: &str = "poll-renewal-token";

    #[derive(Default)]
    struct RecordingStore {
        cancels: Vec<(JobId, u64)>,
        held: Vec<(JobId, u64)>,
        renewed: Mutex<Vec<(JobId, u64)>>,
        /// Epochs returned by successive `get_job` calls, used to script a lease transfer.
        epochs: Mutex<Vec<u64>>,
    }

    impl Store for RecordingStore {
        fn create_job(&self, _spec: &JobSpec, _now: Now) -> Result<(JobRecord, bool), StoreError> {
            unimplemented!()
        }
        fn get_job(&self, id: &JobId) -> Result<JobRecord, StoreError> {
            let mut epochs = self
                .epochs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let epoch = epochs.remove(0);
            Ok(JobRecord {
                id: id.clone(),
                state: JobState::Running,
                spec: JobSpec {
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
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    idle_timeout_secs: None,
                    on_stuck: None,
                    stuck_after_secs: None,
                    idempotency_key: None,
                    after: Vec::new(),
                    collect: lu_proto::CollectMode::Apply,
                    include_failed: false,
                    on_conflict: Default::default(),
                    allow_empty: false,
                    requires: Vec::new(),
                    tolerates: Vec::new(),
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
                    fallbacks: Vec::new(),
                    expect: None,
                    stack_union_paths: Vec::new(),
                    stack_base_paths: Vec::new(),
                    schedule: None,
                },
                attempt: 1,
                node: Some(NodeName::parse("node-a").unwrap()),
                assigned_provider: None,
                assigned_model: None,
                epoch,
                created_at: 1,
                updated_at: 2,
                eligible_since: 1,
                started_at: Some(2),
                finished_at: None,
                failure_class: None,
                summary: None,
                exit_code: None,
                artifacts: Vec::new(),
                cancel_requested: false,
                log_len: 0,
                diff_bytes: None,
                expect_failed: false,
                scrubbed_hits: 0,
                patch_scrubbed: false,
                patch_secret_hits: lu_proto::view::PatchSecretHitsView::default(),
                waiting_on: Vec::new(),
                staged_commit: None,
                blocked_reason: None,
                blocked_since: None,
                parent_job_id: None,
                root_job_id: None,
                depth: 0,
                publish: None,
            })
        }
        fn job_by_key(&self, _key: &str) -> Option<JobRecord> {
            None
        }
        fn list_jobs(
            &self,
            _state: Option<lu_proto::JobState>,
            _limit: usize,
            _after: Option<&JobId>,
        ) -> Result<Vec<JobRecord>, StoreError> {
            unimplemented!()
        }
        fn edit_job(
            &self,
            _id: &JobId,
            _edit: &lu_proto::JobEdit,
            _now: Now,
        ) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn cancel_job(&self, _id: &JobId, _now: Now) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn heartbeat(&self, _hb: &Heartbeat, _now: Now) -> Result<(), StoreError> {
            Ok(())
        }
        fn next_assignment(
            &self,
            _node: &NodeName,
            _now: Now,
            _lease_ms: u64,
        ) -> Result<Option<lu_store::Assignment>, StoreError> {
            Ok(None)
        }
        fn pending_cancels(&self, _node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
            Ok(self.cancels.clone())
        }
        fn held_jobs(&self, _node: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
            Ok(self.held.clone())
        }
        fn start_job(&self, _id: &JobId, _epoch: u64, _now: Now) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn renew_lease(
            &self,
            id: &JobId,
            epoch: u64,
            _now: Now,
            _lease_ms: u64,
        ) -> Result<bool, StoreError> {
            self.renewed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((id.clone(), epoch));
            Ok(false)
        }
        fn reserve_log(
            &self,
            _id: &JobId,
            _epoch: u64,
            _offset: u64,
            _n: u64,
        ) -> Result<u64, StoreError> {
            unimplemented!()
        }
        fn unreserve_log(
            &self,
            _id: &JobId,
            _epoch: u64,
            _length: u64,
            _n: u64,
        ) -> Result<u64, StoreError> {
            unimplemented!()
        }
        fn record_artifact(
            &self,
            _id: &JobId,
            _epoch: u64,
            _name: ArtifactName,
            _bytes: u64,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn record_scrub(
            &self,
            _id: &JobId,
            _epoch: u64,
            _hits: u64,
            _patch: bool,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn record_patch_secret_hits(
            &self,
            _id: &JobId,
            _epoch: u64,
            _hits: &lu_proto::view::PatchSecretHitsView,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn record_publish(
            &self,
            _: &JobId,
            _: &lu_proto::PublishRecord,
            _: Now,
        ) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn complete(
            &self,
            _completion: &Completion,
            _failure: Option<(lu_proto::FailureClass, Option<String>)>,
            _now: Now,
            _max_retries: u32,
            _cordon_ms: u64,
        ) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn expire_leases(&self, _now: Now) -> Result<Vec<JobId>, StoreError> {
            unimplemented!()
        }
        fn requeue_recovering(
            &self,
            _now: Now,
            _grace_ms: u64,
            _max_retries: u32,
        ) -> Result<Vec<JobId>, StoreError> {
            unimplemented!()
        }
        fn recover_on_boot(&self, _now: Now) -> Result<u64, StoreError> {
            unimplemented!()
        }
        fn set_draining(&self, _node: &NodeName, _draining: bool) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn set_agent_drain(&self, _node: &NodeName) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn clear_agent_drain(&self, _node: &NodeName) -> Result<bool, StoreError> {
            unimplemented!()
        }
        fn nodes(
            &self,
            _now: Now,
            _lease_ms: u64,
        ) -> Result<Vec<lu_store::NodeRecord>, StoreError> {
            unimplemented!()
        }
        fn cordons(&self, _node: Option<&NodeName>) -> Result<Vec<lu_store::Cordon>, StoreError> {
            unimplemented!()
        }
        fn set_cordon(
            &self,
            _node: &NodeName,
            _provider: &str,
            _source: lu_store::CordonSource,
            _reason: &str,
            _set_at_unix: i64,
            _until_mono_ms: i64,
        ) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn uncordon(
            &self,
            _node: &NodeName,
            _provider: Option<&str>,
        ) -> Result<Vec<String>, StoreError> {
            unimplemented!()
        }
        fn job_counts(&self, _now: Now, _lease_ms: u64) -> Result<JobCounts, StoreError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn poll_with_pending_cancels_renews_every_held_job() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = JobId::new();
        let other = JobId::new();
        let store = Arc::new(RecordingStore {
            cancels: vec![(cancel.clone(), 3)],
            held: vec![(cancel.clone(), 3), (other.clone(), 7)],
            renewed: Mutex::new(Vec::new()),
            epochs: Mutex::new(Vec::new()),
        });
        let cfg = ControllerConfig {
            listen: "127.0.0.1:0".to_owned(),
            db: tmp.path().join("lu.db").display().to_string(),
            artifacts_dir: tmp.path().join("artifacts").display().to_string(),
            lease_secs: 6,
            max_retries: 1,
            provider_cooldown_secs: 60,
            recovery_grace_secs: 1,
            max_connections: 8,
            max_connections_per_addr: 4,
            local_repo_roots: Vec::new(),
            agents: vec![ControllerAgent {
                name: "node-a".to_owned(),
                token_sha256: sha256_hex(TOKEN),
            }],
            operators: vec![ControllerOperator {
                name: "ops".to_owned(),
                token_sha256: sha256_hex("unused-operator-token"),
                schedule: false,
                ..Default::default()
            }],
            repos: Vec::new(),
            groups: Vec::new(),
            blocked_dependent_ttl: "24h".to_owned(),
            read_per_minute: lu_config::DEFAULT_READ_PER_MINUTE,
        };
        let state = Arc::new(AppState::from_config(&cfg, store.clone()).unwrap());
        let body = json!({"heartbeat": {"node": "node-a", "harness": "idle", "providers": []}, "wait_secs": 0}).to_string();
        let req = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/agent/poll")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let headers = req.headers().clone();
        let peer = axum::Extension(PeerAddr("127.0.0.1:4000".parse::<SocketAddr>().unwrap()));

        let res = poll(State(state), peer, headers, req).await;

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            store
                .renewed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            vec![(cancel, 3), (other, 7)]
        );
    }

    #[tokio::test]
    async fn a_lease_transfer_between_checks_cannot_overwrite_the_new_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let job_id = JobId::new();
        let store = Arc::new(RecordingStore {
            epochs: Mutex::new(vec![1, 2]),
            ..Default::default()
        });
        let cfg = ControllerConfig {
            listen: "127.0.0.1:0".to_owned(),
            db: tmp.path().join("lu.db").display().to_string(),
            artifacts_dir: tmp.path().join("artifacts").display().to_string(),
            lease_secs: 6,
            max_retries: 1,
            provider_cooldown_secs: 60,
            recovery_grace_secs: 1,
            max_connections: 8,
            max_connections_per_addr: 4,
            local_repo_roots: Vec::new(),
            agents: vec![ControllerAgent {
                name: "node-a".to_owned(),
                token_sha256: sha256_hex(TOKEN),
            }],
            operators: vec![ControllerOperator {
                name: "ops".to_owned(),
                token_sha256: sha256_hex("unused-operator-token"),
                schedule: false,
                ..Default::default()
            }],
            repos: Vec::new(),
            groups: Vec::new(),
            blocked_dependent_ttl: "24h".to_owned(),
            read_per_minute: lu_config::DEFAULT_READ_PER_MINUTE,
        };
        let state = Arc::new(AppState::from_config(&cfg, store).unwrap());
        let dir = tmp.path().join("artifacts").join(job_id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("diff.patch");
        std::fs::write(&path, b"newer attempt\n").unwrap();

        let uri = format!("/v1/agent/jobs/{job_id}/artifacts/diff.patch?epoch=1");
        let req = axum::extract::Request::builder()
            .method("PUT")
            .uri(uri)
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(axum::body::Body::from("stale attempt\n"))
            .unwrap();
        let headers = req.headers().clone();
        let peer = axum::Extension(PeerAddr("127.0.0.1:4000".parse::<SocketAddr>().unwrap()));
        let res = put_diff_artifact(
            State(state),
            peer,
            headers,
            Path(job_id.to_string()),
            req.uri().clone(),
            req,
        )
        .await;

        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read(&path).unwrap(), b"newer attempt\n");
    }
}
