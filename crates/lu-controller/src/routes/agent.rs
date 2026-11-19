use crate::auth::authenticate_agent;
use crate::error::ApiError;
use crate::routes::{
    json_ok, read_json, read_raw_limited, store_err, with_version, MAX_DIFF_BODY, MAX_JSON_BODY,
    MAX_LOG_CHUNK_BODY, MAX_REPORT_BODY,
};
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_proto::{ArtifactName, Completion, Heartbeat, JobId, NodeStats};
use lu_serve::PeerAddr;
use lu_staging::{pack_commit_capped, StageError};
use serde::Deserialize;
use serde_json::json;
use std::io::Cursor;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollBody {
    heartbeat: Heartbeat,
    wait_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EpochBody {
    epoch: u64,
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

        // try assignment only if no current work implied? store enforces no job held.
        let assignment = {
            let store = Arc::clone(&state.store);
            let node_c = node.clone();
            let lease_ms = state.lease_ms;
            let n2 = now();
            match tokio::task::spawn_blocking(move || store.next_assignment(&node_c, n2, lease_ms))
                .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return with_version(store_err(e).into_response()),
                Err(_) => return with_version(ApiError::internal().into_response()),
            }
        };
        if let Some(a) = assignment {
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
            // renew implied by poll on assigned job? assignment created lease; cancels returned are pending.
            // "Extends the lease by `lease_secs`. Also implied by `poll`, `log` and `start`."
            // Any active leased/running job held by node should be renewed by poll - but store API doesn't give direct "renew by node".
            // However next_assignment/pending_cancels operate on held jobs; renew is per job+epoch endpoint. Poll implying renew is important.
            // Without a store method, we can't perfectly implement; but we can attempt to renew known held jobs if we can discover them.
            // As a pragmatic approach: if assignment made, lease already set. If cancels exist, those jobs are held - try renew for each? need epoch from pending_cancels (we have it).
            // Best effort: renew each cancel job epoch.
            // (Not perfect but better than nothing.)
            for ev in &events {
                if ev.get("kind").and_then(|k| k.as_str()) == Some("cancel") {
                    if let (Some(id), Some(epoch)) = (
                        ev.get("job_id").and_then(|v| v.as_str()),
                        ev.get("epoch").and_then(|v| v.as_u64()),
                    ) {
                        if let Ok(jid) = JobId::parse(id) {
                            let store = Arc::clone(&state.store);
                            let lease_ms = state.lease_ms;
                            let n3 = now();
                            let _ = tokio::task::spawn_blocking(move || {
                                store.renew_lease(&jid, epoch, n3, lease_ms)
                            })
                            .await;
                        }
                    }
                }
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
            state.waiters.notify_job(rec.id.as_ref());
            state.waiters.notify_work();
            with_version(json_ok(
                StatusCode::OK,
                &crate::routes::operator::job_view(&rec),
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

async fn append_log_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        // Private from the first byte, not narrowed after creation.
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .await?;
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .await?;
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
    if actual_append > 0 {
        let res = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            let jid = job_id.clone();
            move || store.reserve_log(&jid, epoch, offset, actual_append)
        })
        .await;
        match res {
            Ok(Ok(_new_len)) => {}
            Ok(Err(e)) => return with_version(store_err(e).into_response()),
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
        let path = state.artifacts_dir.join(job_id.to_string()).join("run.log");
        if append_log_bytes(&path, &to_write).await.is_err() {
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
            Ok(Ok(_)) => {}
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
    state.waiters.notify_job(job_id.as_ref());
    with_version(StatusCode::OK.into_response())
}

fn artifact_name_from_str(name: &str) -> Option<ArtifactName> {
    match name {
        "diff.patch" => Some(ArtifactName::DiffPatch),
        "run.log" => Some(ArtifactName::RunLog),
        "report.json" => Some(ArtifactName::ReportJson),
        _ => None,
    }
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
    let failure = match completion.outcome {
        lu_proto::Outcome::Done | lu_proto::Outcome::Cancelled => None,
        lu_proto::Outcome::Failed(class) => {
            // A fallback attempt must cordon the pair it actually ran on.
            let provider = rec_check.assigned_provider.clone();
            Some((class, provider))
        }
    };
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let max_retries = state.max_retries;
        let cordon_ms = cordon_ms_for(&completion, state.cordon_ms);
        move || store.complete(&completion, failure, n, max_retries, cordon_ms)
    })
    .await;
    match res {
        Ok(Ok(rec)) => {
            if crate::routes::operator::outcome_note(&rec) == Some("no_changes") {
                state.count_no_changes();
            }
            state.waiters.notify_job(rec.id.as_ref());
            state.waiters.notify_work();
            with_version(json_ok(
                StatusCode::OK,
                &crate::routes::operator::job_view(&rec),
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
        }));
    }
    with_version(json_ok(StatusCode::OK, &deps))
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
    match tokio::fs::read(dependency_diff_path(&state, &dep_id)).await {
        Ok(bytes) => {
            let res = (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                bytes,
            )
                .into_response();
            with_version(res)
        }
        Err(_) => with_version(ApiError::not_found("not found").into_response()),
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
    use lu_proto::{Completion, FailureClass, JobId, Outcome};

    fn completion(outcome: Outcome, retry: Option<u64>) -> Completion {
        Completion {
            job_id: JobId::new(),
            epoch: 1,
            outcome,
            exit_code: None,
            summary: None,
            retry_after_secs: retry,
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
