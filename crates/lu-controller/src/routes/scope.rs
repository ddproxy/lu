//! Job-scoped routes. These are the security boundary for harness-initiated work: the caller must
//! hold an agent token, and the named job must be leased to that agent at exactly that epoch.
//!
//! Nothing here accepts an operator token, reads another job's task text, or accepts a job id the
//! caller cannot prove it owns. Errors deliberately use the same `404 not found` for a missing job
//! and an unrelated one.

use std::collections::HashSet;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_audit::{Data, Event, Field};
use lu_proto::{
    decode_scope_request, short_summary_with_limit, ArtifactName, Choice, CollectMode, JobId,
    JobSpec, OnConflict, ScopeAction, ScopeArtifact, ScopeChild, ScopeDependency,
    ScopeErrorResponse, ScopeRequest, ScopeResponse, ScopeResult, MAX_IDEMPOTENCY_KEY_BYTES,
    MAX_SCOPE_REQUEST_BYTES,
};
use lu_serve::PeerAddr;
use lu_staging::ArtifactFile;
use lu_store::{JobRecord, Now, ScopeLimits, StoreError};

use crate::auth::authenticate_agent;
use crate::error::{ApiError, ErrorCode};
use crate::routes::{json_ok, read_raw_limited, store_err, with_version};
use crate::state::AppState;
use crate::time::now;

const NOT_FOUND: &str = "not found";
/// Audit job id for failures that happen before the lease proves which job the caller owns.
const UNVERIFIED_JOB: &str = "unknown";

fn not_found() -> ApiError {
    ApiError::not_found(NOT_FOUND)
}

fn limit(limit: &'static str) -> ApiError {
    ApiError::limit_exceeded(limit)
}

fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::NotFound => not_found(),
        StoreError::StaleEpoch => ApiError::stale_epoch(),
        other => store_err(other),
    }
}

/// Scope errors use the same untrusted envelope as results. Messages are fixed here; never echo
/// child-supplied strings, even in an error.
fn scope_error_response(error: &ApiError) -> Response {
    let mut body = ScopeErrorResponse::new(error.code.as_str(), limit_message(error));
    if let Some(name) = &error.limit {
        body = body.with_limit(name.clone());
    }
    with_version(json_ok(error.code.status(), &body))
}

async fn get_job(state: &Arc<AppState>, id: &JobId) -> Result<JobRecord, ApiError> {
    let store = Arc::clone(&state.store);
    let id = id.clone();
    match tokio::task::spawn_blocking(move || store.get_job(&id)).await {
        Ok(Ok(record)) => Ok(record),
        Ok(Err(error)) => Err(store_error(error)),
        Err(_) => Err(ApiError::internal()),
    }
}

async fn children(state: &Arc<AppState>, parent: &JobId) -> Result<Vec<JobRecord>, ApiError> {
    let store = Arc::clone(&state.store);
    let parent = parent.clone();
    match tokio::task::spawn_blocking(move || store.children(&parent)).await {
        Ok(Ok(records)) => Ok(records),
        Ok(Err(error)) => Err(store_error(error)),
        Err(_) => Err(ApiError::internal()),
    }
}

fn scope_child(record: &JobRecord, max_summary: usize) -> ScopeChild {
    ScopeChild {
        job_id: record.id.clone(),
        state: record.state.as_str().to_owned(),
        failure_class: record.failure_class.map(|class| class.as_str().to_owned()),
        summary: short_summary_with_limit(record.summary.as_deref(), max_summary),
    }
}

fn scope_dependency(record: &JobRecord, max_summary: usize) -> ScopeDependency {
    ScopeDependency {
        job_id: record.id.clone(),
        idempotency_key: record.spec.idempotency_key.clone(),
        state: record.state.as_str().to_owned(),
        failure_class: record.failure_class.map(|class| class.as_str().to_owned()),
        summary: short_summary_with_limit(record.summary.as_deref(), max_summary),
    }
}

async fn job_context(state: &Arc<AppState>, job: &JobRecord) -> Result<ScopeResult, ApiError> {
    let mut dependencies = Vec::with_capacity(job.spec.after.len());
    for id in &job.spec.after {
        // A dependency named by the job's own spec is the only non-child job data it may see.
        match get_job(state, id).await {
            Ok(dep) => dependencies.push(scope_dependency(&dep, state.scope.max_summary_bytes)),
            Err(error) if error.code == ErrorCode::NotFound => {
                dependencies.push(ScopeDependency {
                    job_id: id.clone(),
                    idempotency_key: None,
                    state: "unknown".to_owned(),
                    failure_class: None,
                    summary: None,
                });
            }
            Err(error) => return Err(error),
        }
    }
    Ok(ScopeResult::JobContext {
        job_id: job.id.clone(),
        state: job.state.as_str().to_owned(),
        spec: Box::new(job.spec.clone()),
        dependencies,
    })
}

fn allowed_choices(parent: &JobRecord) -> Vec<(String, Option<String>)> {
    let provider = parent
        .spec
        .provider
        .clone()
        .or_else(|| parent.assigned_provider.clone())
        .unwrap_or_else(|| lu_proto::ANY_PROVIDER.to_owned());
    let model = parent
        .spec
        .model
        .clone()
        .or_else(|| parent.assigned_model.clone());
    let mut choices = vec![(provider, model)];
    choices.extend(
        parent
            .spec
            .fallbacks
            .iter()
            .map(|choice| (choice.provider.clone(), choice.model.clone())),
    );
    choices
}

fn remaining_timeout(parent: &JobRecord, n: Now) -> Result<u64, ApiError> {
    let elapsed = parent
        .started_at
        .map(|started| n.wall_unix.saturating_sub(started).max(0) as u64)
        .unwrap_or(0);
    let remaining = parent.spec.timeout_secs.saturating_sub(elapsed).max(1);
    Ok(remaining)
}

fn namespaced_key(parent: &JobId, raw: &str) -> Result<String, ApiError> {
    let prefix = format!("scope:{}:", parent);
    let available = MAX_IDEMPOTENCY_KEY_BYTES.saturating_sub(prefix.len());
    if raw.len() > available {
        return Err(ApiError::bad_request("idempotency_key is too long"));
    }
    let key = format!("{prefix}{raw}");
    lu_proto::validate_idempotency_key(&key)
        .map_err(|error| ApiError::unprocessable(error.to_string()))?;
    Ok(key)
}

fn build_child_spec(
    state: &AppState,
    parent: &JobRecord,
    payload: &lu_proto::ScopePayload,
    n: Now,
) -> Result<JobSpec, ApiError> {
    let task = payload.task.as_deref().unwrap_or_default();
    if task.len() > state.scope.max_task_bytes {
        return Err(limit("max_task_bytes"));
    }
    if payload.git_ref.is_some() {
        // The decoder rejects this field for submit_child. Keep the check here too so a future
        // proto change cannot silently reintroduce a child-chosen ref.
        return Err(ApiError::bad_request(
            "git_ref cannot be set by a child job",
        ));
    }
    let repo = payload.repo.as_deref().unwrap_or(&parent.spec.repo);
    if repo != parent.spec.repo && !parent.spec.spawn_repos.iter().any(|r| r == repo) {
        return Err(ApiError::bad_request("repo is not allowed by the parent"));
    }
    let choices = allowed_choices(parent);
    let provider = payload
        .provider
        .clone()
        .unwrap_or_else(|| choices[0].0.clone());
    let mut model = match payload.model.as_deref() {
        Some(model) => {
            if !choices
                .iter()
                .any(|(p, m)| p == &provider && m.as_deref() == Some(model))
            {
                return Err(ApiError::bad_request(
                    "provider/model is not allowed by the parent",
                ));
            }
            Some(model.to_owned())
        }
        None => choices
            .iter()
            .find(|(p, _)| p == &provider)
            .and_then(|(_, m)| m.clone()),
    };
    if !choices.iter().any(|(p, _)| p == &provider) {
        return Err(ApiError::bad_request(
            "provider is not allowed by the parent",
        ));
    }
    if provider == lu_proto::ANY_PROVIDER {
        // `provider = "any"` is a request for the agent's healthy pairs; it cannot carry a fixed
        // model or fallbacks. The parent's assigned model belongs to a concrete provider choice.
        model = None;
    }

    let priority = payload
        .priority
        .map(|value| value.min(parent.spec.priority))
        .unwrap_or(parent.spec.priority);
    let requires = payload.requires.clone().unwrap_or_default();
    if requires
        .iter()
        .any(|label| !parent.spec.requires.contains(label))
    {
        return Err(ApiError::bad_request(
            "requires must be a subset of the parent's",
        ));
    }
    let remaining = remaining_timeout(parent, n)?;
    let timeout = payload
        .timeout_secs
        .map(|value| value.min(remaining))
        .unwrap_or(remaining);
    let idempotency_key = match &payload.idempotency_key {
        Some(raw) => Some(namespaced_key(&parent.id, raw)?),
        None => None,
    };

    let mut spec = parent.spec.clone();
    // The parent may have been a draft that an operator approved. Approval changes the stored
    // state, not the recorded spec, so clearing it here keeps a child out of the draft queue.
    spec.draft = false;
    spec.task = task.to_owned();
    spec.repo = repo.to_owned();
    spec.source.repo = repo.to_owned();
    // A child cannot choose a different ref or base commit: those are operator decisions about
    // the code the tree starts from, not a harness preference.
    spec.git_ref = parent.spec.git_ref.clone();
    spec.source.git_ref = parent.spec.git_ref.clone();
    spec.base_commit = None;
    spec.source.base = None;
    spec.provider = Some(provider.clone());
    spec.model = model;
    spec.priority = priority;
    spec.timeout_secs = timeout;
    spec.idle_timeout_secs = parent
        .spec
        .idle_timeout_secs
        .map(|value| value.min(timeout));
    spec.requires = requires;
    spec.after = payload.after.clone().unwrap_or_default();
    spec.collect = CollectMode::None;
    spec.include_failed = false;
    spec.on_conflict = OnConflict::Fail;
    spec.allow_empty = false;
    spec.expect = None;
    spec.group = None;
    spec.fallbacks = if provider == lu_proto::ANY_PROVIDER {
        Vec::new()
    } else {
        choices
            .into_iter()
            .filter(|(choice, _)| choice != &provider)
            .map(|(provider, model)| Choice { provider, model })
            .collect()
    };
    spec.allow_spawn = parent.spec.spawn_inherit;
    spec.spawn_inherit = parent.spec.spawn_inherit;
    spec.spawn_repos = parent.spec.spawn_repos.clone();
    spec.idempotency_key = idempotency_key;
    spec.validate()
        .map_err(|error| ApiError::unprocessable(error.to_string()))?;
    Ok(spec)
}

async fn submit_child(
    state: &Arc<AppState>,
    parent: &JobRecord,
    payload: &lu_proto::ScopePayload,
) -> Result<ScopeResult, ApiError> {
    // The socket is available for read-only context, but spawning is a separate capability that
    // only an operator can grant. It is never implied by owning a lease.
    if !parent.spec.allow_spawn {
        return Err(ApiError::forbidden("spawn is not allowed for this job"));
    }
    let depth = parent.depth.saturating_add(1);
    if depth > state.scope.max_depth {
        return Err(limit("max_depth"));
    }

    // `after` may name only this job's own children or the dependencies already in its spec.
    let mut allowed_after: HashSet<JobId> = parent.spec.after.iter().cloned().collect();
    for child in children(state, &parent.id).await? {
        allowed_after.insert(child.id.clone());
    }
    for id in payload.after.as_deref().unwrap_or(&[]) {
        if !allowed_after.contains(id) {
            return Err(not_found());
        }
    }

    let n = now();
    let spec = build_child_spec(state, parent, payload, n)?;
    let spawn_token = match state.allow_scope_spawn(&parent.id) {
        Some(token) => token,
        None => return Err(limit("max_spawn_per_minute")),
    };
    let root = parent
        .root_job_id
        .clone()
        .unwrap_or_else(|| parent.id.clone());
    let store = Arc::clone(&state.store);
    let spec_for_store = spec.clone();
    let parent_id = parent.id.clone();
    let limits = ScopeLimits {
        max_children: state.scope.max_children_per_job,
        max_descendants: state.scope.max_descendants_per_root,
    };
    let created = tokio::task::spawn_blocking(move || {
        store.create_scoped_job(&spec_for_store, n, &parent_id, &root, depth, limits)
    })
    .await;
    let (child, created_new) = match created {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => {
            state.refund_scope_spawn(&parent.id, spawn_token);
            return Err(store_error(error));
        }
        Err(_) => {
            state.refund_scope_spawn(&parent.id, spawn_token);
            return Err(ApiError::internal());
        }
    };
    if child.parent_job_id.as_ref() != Some(&parent.id) {
        // A namespaced key should never land on another tree; treat a conflict as an error rather
        // than returning an unrelated job's id.
        state.refund_scope_spawn(&parent.id, spawn_token);
        return Err(ApiError::conflict("idempotency key belongs to another job"));
    }
    let _ = created_new;
    state.waiters.notify_work();
    Ok(ScopeResult::SubmitChild { job_id: child.id })
}

async fn direct_child(
    state: &Arc<AppState>,
    parent: &JobId,
    child_id: &JobId,
) -> Result<JobRecord, ApiError> {
    let child = get_job(state, child_id).await?;
    if child.parent_job_id.as_ref() != Some(parent) {
        // Identical to a missing id: no existence leak for siblings, parents or unrelated jobs.
        return Err(not_found());
    }
    Ok(child)
}

async fn child_artifact(
    state: &Arc<AppState>,
    child: &JobRecord,
    name: &str,
) -> Result<ScopeResult, ApiError> {
    let (file, artifact_name) = match name {
        "diff.patch" => (ArtifactFile::DiffPatch, ArtifactName::DiffPatch),
        "report.json" => (ArtifactFile::ReportJson, ArtifactName::ReportJson),
        // The decoder rejects anything else; keep the check at the boundary for defence in depth.
        _ => return Err(not_found()),
    };
    // Never serve a file that the store does not record for this child. This blocks a planted
    // artifact path from being used to read another job's file.
    if !child.artifacts.contains(&artifact_name) {
        return Err(not_found());
    }
    // The promise is scrubbed variants only. Refuse when the operator disabled scrubbing rather
    // than returning bytes that were stored without it.
    if !state.scrubber.enabled() {
        return Err(not_found());
    }
    let dir = state.artifact_dir().map_err(|_| ApiError::internal())?;
    let path = dir.root().join(child.id.to_string()).join(file.file_name());
    let cap = state.scope.max_artifact_read_bytes;
    let job_id = child.id.clone();
    let file_name = file.file_name().to_owned();
    let read = tokio::task::spawn_blocking(move || {
        let meta = std::fs::symlink_metadata(&path).map_err(|_| not_found())?;
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err(not_found());
        }
        if meta.len() as usize > cap {
            return Err(limit("max_artifact_read_bytes"));
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        opts.custom_flags(libc::O_NOFOLLOW);
        let mut handle = opts.open(&path).map_err(|_| not_found())?;
        let opened = handle.metadata().map_err(|_| not_found())?;
        if !opened.is_file() {
            return Err(not_found());
        }
        if opened.len() as usize > cap {
            return Err(limit("max_artifact_read_bytes"));
        }
        let mut bytes = Vec::with_capacity(opened.len() as usize);
        handle
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::internal())?;
        if bytes.len() > cap {
            return Err(limit("max_artifact_read_bytes"));
        }
        Ok::<_, ApiError>(bytes)
    })
    .await;
    let bytes = match read {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(ApiError::internal()),
    };
    // Re-scrub at read time as well: the on-disk bytes may predate a scrub rule, and a reader must
    // never be able to bypass redaction by changing controller configuration after upload. Patches
    // are scrubbed here even when `scrub_patch` is off, since that option only controls storage.
    let scrubbed = if file == ArtifactFile::DiffPatch {
        state.scrubber.scrub_patch_for_read(&bytes)
    } else {
        state.scrubber.scrub(&bytes, false)
    };
    if scrubbed.data.len() > cap {
        return Err(limit("max_artifact_read_bytes"));
    }
    Ok(ScopeResult::GetChildArtifact {
        artifact: ScopeArtifact {
            job_id,
            name: file_name,
            content: String::from_utf8_lossy(&scrubbed.data).into_owned(),
            truncated: false,
            scrubbed: true,
        },
    })
}

async fn process(
    state: &Arc<AppState>,
    request: &ScopeRequest,
    job: &JobRecord,
) -> Result<ScopeResult, ApiError> {
    let payload = request.payload.as_ref();
    match request.action {
        ScopeAction::JobContext => job_context(state, job).await,
        ScopeAction::ListChildren => {
            let records = children(state, &job.id).await?;
            Ok(ScopeResult::ListChildren {
                children: records
                    .iter()
                    .map(|record| scope_child(record, state.scope.max_summary_bytes))
                    .collect(),
            })
        }
        ScopeAction::SubmitChild => {
            let Some(payload) = payload else {
                return Err(ApiError::bad_request("missing payload"));
            };
            submit_child(state, job, payload).await
        }
        ScopeAction::GetChild | ScopeAction::CancelChild => {
            let child_id = payload
                .and_then(|p| p.child.as_ref())
                .ok_or_else(|| ApiError::bad_request("missing child"))?;
            let child = direct_child(state, &job.id, child_id).await?;
            if request.action == ScopeAction::GetChild {
                return Ok(ScopeResult::GetChild {
                    child: scope_child(&child, state.scope.max_summary_bytes),
                });
            }
            let changed =
                crate::routes::cancel_job_with_descendants(state, &child.id, false, true, None)
                    .await?;
            for id in &changed {
                state.waiters.notify_job(id.as_ref());
            }
            let updated = get_job(state, &child.id).await?;
            Ok(ScopeResult::CancelChild {
                job_id: child.id,
                state: updated.state.as_str().to_owned(),
            })
        }
        ScopeAction::GetChildArtifact => {
            let payload = payload.ok_or_else(|| ApiError::bad_request("missing payload"))?;
            let child_id = payload
                .child
                .as_ref()
                .ok_or_else(|| ApiError::bad_request("missing child"))?;
            let name = payload
                .artifact
                .as_deref()
                .ok_or_else(|| ApiError::bad_request("missing artifact"))?;
            let child = direct_child(state, &job.id, child_id).await?;
            child_artifact(state, &child, name).await
        }
    }
}

fn result_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::LimitExceeded => "limit_exceeded",
        ErrorCode::NotFound => "not_found",
        ErrorCode::BadRequest | ErrorCode::Unprocessable | ErrorCode::TooLarge => "bad_request",
        ErrorCode::StaleEpoch => "stale_epoch",
        ErrorCode::Precondition => "precondition_failed",
        ErrorCode::IdempotencyConflict => "conflict",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Unauthorized => "unauthorized",
        ErrorCode::Forbidden => "forbidden",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::Internal => "internal",
    }
}

/// `job_id` is either the lease-verified job id or `UNVERIFIED_JOB`; a caller-supplied id must
/// never become an audit principal before `leased_job` has validated it.
fn audit_scope(
    state: &Arc<AppState>,
    action: ScopeAction,
    outcome: &str,
    job_id: &str,
    node: &str,
    epoch: u64,
) {
    state.record_audit(
        Event::ScopeAction,
        Some(job_id),
        Some(node),
        &format!("job:{job_id}"),
        Data::new()
            .text(Field::Source, lu_proto::SCOPE_SOURCE)
            .text(Field::Mode, action.as_str())
            .text(Field::Outcome, outcome)
            .unsigned(Field::Epoch, epoch),
    );
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(route): Path<String>,
    req: Request,
) -> Response {
    let route_label = ScopeAction::parse(&route)
        .map(ScopeAction::as_str)
        .unwrap_or("unknown");
    let node = match authenticate_agent(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(node) => node,
        Err(error) => {
            state.record_scope_request(route_label, result_label(error.code));
            return with_version(error.into_response());
        }
    };
    let Some(action) = ScopeAction::parse(&route) else {
        state.record_scope_request("unknown", "not_found");
        return with_version(ApiError::not_found(NOT_FOUND).into_response());
    };

    let bytes = match read_raw_limited(req, MAX_SCOPE_REQUEST_BYTES + 256).await {
        Ok(bytes) => bytes,
        Err(error) => {
            state.record_scope_request(action.as_str(), result_label(error.code));
            return with_version(error.into_response());
        }
    };
    let request = match decode_scope_request(&bytes) {
        Ok(request) => request,
        Err(_) => {
            state.record_scope_request(action.as_str(), "bad_request");
            return scope_error_response(&ApiError::bad_request("invalid scope request"));
        }
    };
    if request.action != action {
        state.record_scope_request(action.as_str(), "bad_request");
        return scope_error_response(&ApiError::bad_request("route does not match action"));
    }
    if !state.scope.enabled {
        let error = limit("scope_enabled");
        state.record_scope_request(action.as_str(), "limit_exceeded");
        state.record_scope_limit("scope_enabled");
        audit_scope(
            &state,
            action,
            "limit_exceeded",
            UNVERIFIED_JOB,
            node.as_str(),
            request.epoch,
        );
        return scope_error_response(&error);
    }

    let n = now();
    let store = Arc::clone(&state.store);
    let job_id = request.job_id.clone();
    let epoch = request.epoch;
    let node_for_store = node.clone();
    let lease_ms = state.lease_ms;
    let leased = tokio::task::spawn_blocking(move || {
        store.leased_job(&job_id, &node_for_store, epoch, n, lease_ms)
    })
    .await;
    let job = match leased {
        Ok(Ok(job)) => job,
        Ok(Err(StoreError::NotFound)) => {
            let error = not_found();
            state.record_scope_request(action.as_str(), "not_found");
            audit_scope(
                &state,
                action,
                "not_found",
                UNVERIFIED_JOB,
                node.as_str(),
                epoch,
            );
            return scope_error_response(&error);
        }
        Ok(Err(StoreError::StaleEpoch)) => {
            let error = ApiError::stale_epoch();
            state.record_scope_request(action.as_str(), "stale_epoch");
            audit_scope(
                &state,
                action,
                "stale_epoch",
                UNVERIFIED_JOB,
                node.as_str(),
                epoch,
            );
            return scope_error_response(&error);
        }
        Ok(Err(error)) => {
            let error = store_error(error);
            let outcome = result_label(error.code);
            state.record_scope_request(action.as_str(), outcome);
            audit_scope(
                &state,
                action,
                outcome,
                UNVERIFIED_JOB,
                node.as_str(),
                epoch,
            );
            return scope_error_response(&error);
        }
        Err(_) => {
            state.record_scope_request(action.as_str(), "internal");
            audit_scope(
                &state,
                action,
                "internal",
                UNVERIFIED_JOB,
                node.as_str(),
                epoch,
            );
            return scope_error_response(&ApiError::internal());
        }
    };

    let result = process(&state, &request, &job).await;
    let outcome = match &result {
        Ok(_) => "ok",
        Err(error) => result_label(error.code),
    };
    state.record_scope_request(action.as_str(), outcome);
    if let Err(error) = &result {
        if error.code == ErrorCode::LimitExceeded {
            if let Some(limit) = &error.limit {
                state.record_scope_limit(
                    lu_proto::SCOPE_LIMITS
                        .iter()
                        .find(|name| **name == limit)
                        .copied()
                        .unwrap_or("scope"),
                );
            }
        }
        audit_scope(
            &state,
            action,
            outcome,
            job.id.as_str(),
            node.as_str(),
            request.epoch,
        );
        return scope_error_response(error);
    }

    audit_scope(
        &state,
        action,
        outcome,
        job.id.as_str(),
        node.as_str(),
        request.epoch,
    );
    let result = result.expect("successful scope result");
    let status = if action == ScopeAction::SubmitChild {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    with_version(json_ok(status, &ScopeResponse::new(result)))
}

/// Fixed messages only. Even the display text of a scoped error must not echo child task bytes.
fn limit_message(error: &ApiError) -> String {
    match error.code {
        ErrorCode::LimitExceeded => error
            .limit
            .as_deref()
            .map(|name| format!("limit exceeded: {name}"))
            .unwrap_or_else(|| "limit exceeded".to_owned()),
        ErrorCode::NotFound => NOT_FOUND.to_owned(),
        ErrorCode::StaleEpoch => "stale epoch".to_owned(),
        ErrorCode::Precondition => "precondition failed".to_owned(),
        ErrorCode::IdempotencyConflict => "idempotency conflict".to_owned(),
        ErrorCode::Unauthorized => "unauthorized".to_owned(),
        ErrorCode::Forbidden => "forbidden".to_owned(),
        ErrorCode::Conflict => "conflict".to_owned(),
        ErrorCode::BadRequest => "bad request".to_owned(),
        ErrorCode::Unprocessable => "unprocessable".to_owned(),
        ErrorCode::TooLarge => "too large".to_owned(),
        ErrorCode::RateLimited => "rate limited".to_owned(),
        ErrorCode::Internal => "internal error".to_owned(),
    }
}
