use crate::auth::authenticate_operator;
use crate::error::ApiError;
use crate::limits::SYNC_PER_TOKEN;
use crate::read_api::{
    authenticate_read, etag_for, job_summary, json_with_etag, parse_job_filter, scan_jobs,
    wants_summary, JobsPage,
};
use crate::routes::file_body::{file_len, into_response, stream_range};
use crate::routes::{
    json_ok, json_value_ok, parse_query, read_raw_limited, store_err, with_version, MAX_JSON_BODY,
};
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use lu_audit::{Data, Event, Field};
use lu_auth::sha256_hex;
use lu_proto::{
    redact_repo, valid_group, validate_idempotency_key, ArtifactName, CordonView, JobEdit, JobId,
    JobSpec, JobView, NodeName, NodeView, WorkspaceCleanResult,
};
use lu_serve::PeerAddr;
use lu_store::{
    is_node_health_reason, missing_labels, waiting_reason_with_groups_and_limits, Cordon,
    CordonSource, JobRecord, NodeRecord, Now, RequestKeyReservation,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
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
pub(crate) fn seconds(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn query_flag(query: &HashMap<String, String>, name: &str) -> bool {
    query
        .get(name)
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

/// Reads an unsigned query parameter. An absent parameter uses the caller's default; a present
/// but unparseable one is a 400 in the standard error shape instead of a silent default.
pub(crate) fn query_u64(
    query: &HashMap<String, String>,
    name: &str,
) -> Result<Option<u64>, ApiError> {
    match query.get(name) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<u64>()
            .map(Some)
            .map_err(|_| ApiError::bad_request(format!("invalid {name}"))),
    }
}

/// HTTP request keys expire after a day. The table is pruned on each claim for that key.
const REQUEST_KEY_TTL_SECS: u64 = 24 * 60 * 60;

/// A write's observation of a job body, used to make `If-Match` conditional inside the store
/// transaction. The ETag comes from the same body a GET would return.
#[derive(Debug, Clone, Copy)]
struct JobGuard {
    updated_at: i64,
    has_dependencies: bool,
}

pub(crate) fn outcome_data(
    state: &AppState,
    operator: &str,
    operation: &str,
    outcome: &str,
) -> Data {
    Data::new()
        .text(Field::Principal, operator)
        .text(Field::Role, role_name(state, operator))
        .text(Field::Operation, operation)
        .text(Field::Outcome, outcome)
}

pub(crate) fn audit_precondition(state: &AppState, operator: &str, operation: &str) {
    state.record_audit(
        Event::MutationPreconditionFailed,
        None,
        None,
        operator,
        outcome_data(state, operator, operation, "precondition_failed"),
    );
}

/// Reads a strong `If-Match`. Weak validators are accepted for compatibility with GET's weak
/// comparison, but only as an exact match after stripping `W/`.
fn if_match_header(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    match headers.get(header::IF_MATCH) {
        None => Ok(None),
        Some(value) => value
            .to_str()
            .map(|v| Some(v.trim().to_owned()))
            .map_err(|_| ApiError::precondition("invalid If-Match")),
    }
}

async fn job_if_match_guard(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    job_id: &JobId,
    operation: &str,
    operator: &str,
) -> Result<Option<JobGuard>, ApiError> {
    let Some(expected) = if_match_header(headers)? else {
        return Ok(None);
    };
    let store = Arc::clone(&state.store);
    let id = job_id.clone();
    let rec = tokio::task::spawn_blocking(move || store.get_job(&id))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(store_err)?;
    let view = job_view_for(state, &rec).await?;
    let body = serde_json::to_string(&view).map_err(|_| ApiError::internal())?;
    let etag = etag_for(&body);
    if expected != "*" && expected.trim_start_matches("W/") != etag {
        state.record_audit(
            Event::MutationPreconditionFailed,
            Some(job_id.as_str()),
            None,
            operator,
            outcome_data(state, operator, operation, "precondition_failed"),
        );
        return Err(ApiError::precondition(
            "job changed since the ETag was read",
        ));
    }
    Ok(Some(JobGuard {
        updated_at: rec.updated_at,
        has_dependencies: !rec.waiting_on.is_empty(),
    }))
}

/// For mutations with no single resource representation, `*` asserts only that the caller made a
/// conditional request. Other validators cannot be checked and are refused, not ignored.
pub(crate) fn star_if_match(
    state: &AppState,
    headers: &HeaderMap,
    operation: &str,
    operator: &str,
) -> Result<(), ApiError> {
    if let Some(expected) = if_match_header(headers)? {
        if expected != "*" {
            audit_precondition(state, operator, operation);
            return Err(ApiError::precondition(
                "this mutation supports only If-Match: *",
            ));
        }
    }
    Ok(())
}

fn dry_run_requested(uri: &Uri) -> Result<bool, ApiError> {
    let query = parse_query(uri);
    match query.get("dry_run").map(String::as_str) {
        None | Some("0") | Some("false") => Ok(false),
        Some("1") | Some("true") => Ok(true),
        Some(_) => Err(ApiError::bad_request("invalid dry_run")),
    }
}

fn request_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    match headers.get("Idempotency-Key") {
        None => Ok(None),
        Some(value) => {
            let key = value
                .to_str()
                .map_err(|_| ApiError::bad_request("invalid Idempotency-Key"))?;
            validate_idempotency_key(key)
                .map_err(|_| ApiError::bad_request("invalid Idempotency-Key"))?;
            Ok(Some(key.to_owned()))
        }
    }
}

fn key_fingerprint(operation: &str, body: &[u8]) -> String {
    let mut input = operation.as_bytes().to_vec();
    input.push(0);
    input.extend_from_slice(body);
    sha256_hex(&String::from_utf8_lossy(&input))
}

/// Returns the stored response body when this key already completed the same operation.
async fn claim_request_key(
    state: &Arc<AppState>,
    operator: &str,
    key: &str,
    operation: &str,
    fingerprint: &str,
) -> Result<Option<Value>, ApiError> {
    let store = Arc::clone(&state.store);
    let operator = operator.to_owned();
    let key = key.to_owned();
    let operation = operation.to_owned();
    let fingerprint = fingerprint.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        store.reserve_request_key(
            &operator,
            &key,
            &operation,
            &fingerprint,
            REQUEST_KEY_TTL_SECS,
        )
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|error| match error {
        lu_store::StoreError::Conflict { .. } => ApiError::idempotency_conflict(),
        other => store_err(other),
    })?;
    Ok(match result {
        RequestKeyReservation::Reserved => None,
        RequestKeyReservation::Replayed(body) => {
            serde_json::from_str(&body).ok().filter(Value::is_object)
        }
    })
}

async fn complete_request_key(
    state: &Arc<AppState>,
    operator: &str,
    key: &str,
    status: StatusCode,
    body: &Value,
) {
    let store = Arc::clone(&state.store);
    let operator = operator.to_owned();
    let key = key.to_owned();
    let response = json!({ "status": status.as_u16(), "body": body });
    let _ =
        tokio::task::spawn_blocking(move || store.complete_request_key(&operator, &key, &response))
            .await;
}

async fn release_request_key(state: &Arc<AppState>, operator: &str, key: &str) {
    let store = Arc::clone(&state.store);
    let operator = operator.to_owned();
    let key = key.to_owned();
    let _ = tokio::task::spawn_blocking(move || store.release_request_key(&operator, &key)).await;
}

/// Reconstitutes a replayed response, including its original status code.
fn replayed_response(stored: &Value) -> Response {
    let status = stored
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK);
    json_value_ok(status, stored.get("body").cloned().unwrap_or(Value::Null))
}

pub fn job_view(r: &JobRecord) -> JobView {
    job_view_with(r, None, lu_config::DEFAULT_BLOCKED_DEPENDENT_TTL_SECS)
}

/// Maps a store record into a view using the configured blocked-dependent deadline.
pub fn job_view_with_ttl(r: &JobRecord, ttl_secs: u64) -> JobView {
    job_view_with(r, None, ttl_secs)
}

fn job_view_with(r: &JobRecord, reason: Option<String>, ttl_secs: u64) -> JobView {
    // Older rows can hold a repo with credentials, query or fragment. Redact on every view,
    // including single-job reads, lists, retries and agent replies, without changing the stored
    // spec the agent still needs for its fetch.
    let mut spec = r.spec.clone();
    spec.repo = redact_repo(&spec.repo).into_owned();
    spec.source.repo = redact_repo(&spec.source.repo).into_owned();
    JobView {
        id: r.id.clone(),
        state: r.state,
        spec,
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
        failure_reason: r.failure_reason,
        excluded_nodes: r.excluded_nodes.clone(),
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
        parent_job_id: r.parent_job_id.clone(),
        root_job_id: r.root_job_id.clone(),
        depth: r.depth,
        waiting_reason: reason,
        staged_commit: r.staged_commit.clone(),
        blocked_reason: r.blocked_reason.clone(),
        blocked_until: r
            .blocked_since
            .map(|since| since.saturating_add(i64::try_from(ttl_secs).unwrap_or(i64::MAX))),
        outcome_note: outcome_note(r).map(str::to_owned),
        diff_bytes: r.state.is_terminal().then(|| r.diff_bytes.unwrap_or(0)),
        scrubbed_hits: r.scrubbed_hits,
        patch_scrubbed: r.patch_scrubbed,
        patch_secret_hits: r.patch_secret_hits.clone(),
        warnings: None,
        audit_write_errors: None,
        publish: r.publish.clone(),
        publish_target: None,
        schedule: r.spec.schedule.clone(),
        waiting_detail: Vec::new(),
    }
}

/// Structured reasons from the same admission evaluator dispatch uses. A job without a schedule
/// block, or a controller without scheduling configured, has no schedule detail.
fn schedule_waiting_detail(state: &AppState, rec: &JobRecord) -> Vec<lu_schedule::BlockReason> {
    let (Some(schedule), Some(job)) = (state.schedule(), rec.spec.schedule.clone()) else {
        return Vec::new();
    };
    let request = lu_schedule::AdmissionRequest {
        now: now().wall_unix,
        eligible_since: rec.eligible_since,
        timeout_secs: rec.spec.timeout_secs as i64,
        job,
        node: rec.node.as_ref().map(|node| node.as_str().to_owned()),
        group: rec.spec.group.clone(),
        provider: rec.assigned_provider.clone(),
        model: rec.assigned_model.clone(),
    };
    match schedule.admit(&request) {
        Ok(lu_schedule::Admission::Blocked(blocked)) => {
            for reason in &blocked.reasons {
                state.record_schedule_wait(reason.constraint.as_str());
            }
            blocked.reasons
        }
        _ => Vec::new(),
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
    let reason = waiting_reason_with_groups_and_limits(
        rec,
        &nodes,
        &counts,
        &state.group_limits(),
        &state.dispatch_limits(),
    );
    let mut view = job_view_with(rec, reason, state.blocked_dependent_ttl_secs);
    if let Some(reason) = view
        .waiting_reason
        .as_deref()
        .filter(|r| is_node_health_reason(r))
    {
        view.blocked_reason = Some(reason.to_owned());
    }
    view.waiting_detail = schedule_waiting_detail(state, rec);
    view.audit_write_errors = Some(state.audit_write_errors());
    view.publish_target = crate::publish::target_text(state, rec);
    Ok(view)
}

fn job_views(
    jobs: &[JobRecord],
    nodes: &[NodeRecord],
    counts: &HashMap<String, usize>,
    limits: &HashMap<String, usize>,
    dispatch_limits: &lu_store::DispatchLimits,
    blocked_ttl_secs: u64,
) -> Vec<JobView> {
    jobs.iter()
        .map(|job| {
            let mut view = job_view_with(
                job,
                waiting_reason_with_groups_and_limits(job, nodes, counts, limits, dispatch_limits),
                blocked_ttl_secs,
            );
            if let Some(reason) = view
                .waiting_reason
                .as_deref()
                .filter(|r| is_node_health_reason(r))
            {
                view.blocked_reason = Some(reason.to_owned());
            }
            view
        })
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
    min_disk_free_pct: u8,
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
        drain_origin: n.draining.then(|| n.drain_origin.as_str().to_owned()),
        harness: Some(n.heartbeat.harness),
        harness_ok: n.heartbeat.harness_ok,
        disk_free_pct: n.heartbeat.disk_free_pct,
        min_disk_free_pct: Some(min_disk_free_pct),
        providers: n.heartbeat.providers.clone(),
        provider_models: n.heartbeat.provider_models.clone(),
        labels: n.heartbeat.labels.clone(),
        label_sources: n.heartbeat.label_sources.clone(),
        taints: n.heartbeat.taints.clone(),
        current_job: n.current_job.clone(),
        cordon_details: cordons,
        reiki: n.heartbeat.reiki.clone(),
        load1: host.map(|host| host.load1),
        mem_available_kb: host.map(|host| host.mem_available_kb),
        mem_total_kb: host.map(|host| host.mem_total_kb),
        stuck: u64::try_from(stuck).unwrap_or(0),
        workspace_bytes: stats.map_or(0, |s| s.workspace_bytes),
        workspace_count: stats.map_or(0, |s| s.workspace_count),
        workspace_reclaimed_bytes_total: stats.map_or(0, |s| s.workspace_reclaimed_bytes_total),
        workspace_evictions_total: stats.map_or(0, |s| s.workspace_evictions_total),
        workspace_delete_errors_total: stats.map_or(0, |s| s.workspace_delete_errors_total),
        gc_removed_total: stats.map_or(0, |s| s.gc_removed_total),
        gc_bytes_freed_total: stats.map_or(0, |s| s.gc_bytes_freed_total),
        workspaces: stats.map_or_else(Vec::new, |s| s.workspaces.clone()),
    }
}

/// The first job field that widens what a job may do, for tokens configured with
/// `restrict_fields`: spawning children, publishing to the source branch, stacking paths.
fn restricted_field(spec: &JobSpec) -> Option<&'static str> {
    if spec.allow_spawn {
        return Some("allow_spawn");
    }
    if spec.spawn_inherit {
        return Some("spawn_inherit");
    }
    if !spec.spawn_repos.is_empty() {
        return Some("spawn_repos");
    }
    if !spec.stack_union_paths.is_empty() {
        return Some("stack_union_paths");
    }
    if !spec.stack_base_paths.is_empty() {
        return Some("stack_base_paths");
    }
    if let Some(publish) = &spec.publish {
        if publish.allow_source_branch {
            return Some("publish.allow_source_branch");
        }
        if publish.target == lu_proto::PublishTarget::SourceBranch {
            return Some("publish.target");
        }
    }
    None
}

/// The role an operator's token carries, as a fixed audit code.
fn role_name(state: &AppState, operator: &str) -> &'static str {
    match state.auth.profile(operator).map(|p| p.role) {
        Some(lu_config::OperatorRole::Viewer) => "viewer",
        Some(lu_config::OperatorRole::Admin) => "admin",
        _ => "operator",
    }
}

pub async fn create_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    uri: Uri,
    req: axum::extract::Request,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(o) => o,
        Err(e) => return with_version(e.into_response()),
    };
    let operation = "create";
    let conditional = headers.contains_key(header::IF_MATCH);
    let dry_run = match dry_run_requested(&uri) {
        Ok(value) => value,
        Err(error) => return with_version(error.into_response()),
    };
    if let Err(error) = star_if_match(&state, &headers, operation, &operator) {
        return with_version(error.into_response());
    }
    let key = match request_key(&headers) {
        Ok(key) => key,
        Err(error) => return with_version(error.into_response()),
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
    if state.auth.restricts_fields(&operator) {
        if let Some(field) = restricted_field(&spec) {
            return with_version(
                ApiError::forbidden(format!("this token may not set {field}")).into_response(),
            );
        }
    }
    let fingerprint = key_fingerprint(operation, &bytes);
    if dry_run {
        state.record_audit(
            Event::MutationDryRun,
            None,
            None,
            &operator,
            outcome_data(&state, &operator, operation, "dry_run"),
        );
        return with_version(json_ok(
            StatusCode::OK,
            &json!({ "dry_run": true, "operation": operation, "draft": spec.draft }),
        ));
    }
    if let Some(repo) = state.mirror.repo_for_url(&spec.repo) {
        let name = repo.name.clone();
        let git_ref = spec.git_ref.clone();
        let mirror = Arc::clone(&state.mirror);
        let resolved =
            tokio::task::spawn_blocking(move || mirror.ensure_resolved(&name, &git_ref)).await;
        match resolved {
            Ok(Ok(commit)) => {
                spec.base_commit = Some(commit.clone());
                spec.source.base = Some(commit);
            }
            Ok(Err(e)) if e.is_unknown_ref() => {
                return with_version(ApiError::unprocessable(e.to_string()).into_response())
            }
            Ok(Err(e)) => {
                tracing::error!(repo = %spec.repo, error = %e, "could not mirror repository");
                return with_version(ApiError::internal().into_response());
            }
            Err(_) => return with_version(ApiError::internal().into_response()),
        }
    }
    if let Some(idempotency_key) = &key {
        match claim_request_key(&state, &operator, idempotency_key, operation, &fingerprint).await {
            Ok(Some(stored)) => {
                state.record_audit(
                    Event::MutationReplay,
                    None,
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "replayed"),
                );
                return with_version(replayed_response(&stored));
            }
            Ok(None) => {}
            Err(error) => return with_version(error.into_response()),
        }
    }
    let submit_data = outcome_data(&state, &operator, operation, "applied")
        .text(Field::Provider, spec.provider.as_deref().unwrap_or("any"))
        .text(Field::Model, spec.model.as_deref().unwrap_or(""));
    let collect_applies = spec.collect.applies_diffs();
    let collect_mode = spec.collect.as_str();
    let is_draft = spec.draft;
    let max_total = state.drafts.max_total;
    let max_per_source = state.drafts.max_per_source;
    let now = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || {
            if is_draft {
                store.create_draft_job(&spec, now, max_total, max_per_source)
            } else if conditional {
                store.create_job_if_absent(&spec, now)
            } else {
                store.create_job(&spec, now)
            }
        }
    })
    .await;
    let (rec, created) = match res {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            if let Some(idempotency_key) = &key {
                let store = Arc::clone(&state.store);
                let operator = operator.clone();
                let idempotency_key = idempotency_key.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    store.release_request_key(&operator, &idempotency_key)
                })
                .await;
            }
            if matches!(e, lu_store::StoreError::Precondition) {
                state.record_audit(
                    Event::MutationPreconditionFailed,
                    None,
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "precondition_failed"),
                );
            }
            return with_version(store_err(e).into_response());
        }
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
    let response_value = serde_json::to_value(&body).unwrap_or(Value::Null);
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    if let Some(idempotency_key) = &key {
        complete_request_key(&state, &operator, idempotency_key, status, &response_value).await;
    }
    if created {
        state.record_audit(
            Event::Submitted,
            Some(rec.id.as_str()),
            None,
            &operator,
            submit_data,
        );
        if collect_applies {
            state.record_audit(
                Event::CollectApplied,
                Some(rec.id.as_str()),
                None,
                &operator,
                Data::new().text(Field::Mode, collect_mode),
            );
        }
        state.waiters.notify_work();
        with_version(json_ok(status, &body))
    } else {
        state.record_audit(
            Event::MutationReplay,
            Some(rec.id.as_str()),
            None,
            &operator,
            outcome_data(&state, &operator, operation, "replayed"),
        );
        with_version(json_ok(status, &body))
    }
}

/// Runtime cap body. Unknown keys are refused so a typo cannot silently leave a batch uncapped.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupLimitRequest {
    max_running: usize,
}

pub async fn set_group_limit(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(group): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(_) => return with_version(ApiError::unauthorized().into_response()),
    };
    if let Err(error) = star_if_match(&state, &headers, "group_limit", &operator) {
        return with_version(error.into_response());
    }
    if !valid_group(&group) {
        return with_version(ApiError::bad_request("invalid group name").into_response());
    }
    let bytes = match read_raw_limited(req, MAX_JSON_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => return with_version(error.into_response()),
    };
    let body: GroupLimitRequest = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => {
            return with_version(ApiError::unprocessable(error.to_string()).into_response())
        }
    };
    if let Err(error) = state.set_group_limit(&group, body.max_running) {
        return with_version(ApiError::bad_request(error).into_response());
    }
    state.record_audit(
        Event::GroupLimit,
        None,
        None,
        &operator,
        outcome_data(&state, &operator, "group_limit", "applied")
            .text(Field::Source, role_name(&state, &operator))
            .unsigned(Field::Count, body.max_running as u64),
    );
    with_version(json_value_ok(
        StatusCode::OK,
        json!({ "group": group, "max_running": body.max_running }),
    ))
}

pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
    }
    let q = crate::routes::parse_query(&uri);
    let summary = match wants_summary(&q) {
        Ok(summary) => summary,
        Err(error) => return with_version(error.into_response()),
    };
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
        let jobs: Vec<JobRecord> = rec.into_iter().collect();
        return with_version(
            match render_job_page(&state, &headers, &jobs, summary, None).await {
                Ok(response) => response,
                Err(error) => error.into_response(),
            },
        );
    }
    let limit = match query_u64(&q, "limit") {
        Ok(value) => usize::try_from(value.unwrap_or(200))
            .unwrap_or(usize::MAX)
            .min(200),
        Err(error) => return with_version(error.into_response()),
    };
    let filter = match parse_job_filter(&q, &uri) {
        Ok(filter) => filter,
        Err(error) => return with_version(error.into_response()),
    };
    let after = match q.get("after") {
        Some(a) if !a.is_empty() => match JobId::parse(a) {
            Ok(id) => Some(id),
            Err(_) => return with_version(ApiError::bad_request("invalid cursor").into_response()),
        },
        _ => None,
    };
    // A single state is filtered by the store; several are checked per row by the scan.
    let store_state = match filter.states.as_slice() {
        [only] => Some(*only),
        _ => None,
    };
    let (jobs, last) = match scan_jobs(&state, store_state, &filter, limit, after).await {
        Ok(page) => page,
        Err(error) => return with_version(error.into_response()),
    };
    let next = last.map(|j| j.to_string());
    with_version(
        match render_job_page(&state, &headers, &jobs, summary, next).await {
            Ok(response) => response,
            Err(error) => error.into_response(),
        },
    )
}

/// Renders a page of jobs in the full or summary shape, with an ETag over the body.
async fn render_job_page(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    jobs: &[JobRecord],
    summary: bool,
    next: Option<String>,
) -> Result<Response, ApiError> {
    if summary {
        let page = JobsPage {
            jobs: jobs
                .iter()
                .map(|job| job_summary(job, &state.scrubber))
                .collect::<Vec<_>>(),
            next,
        };
        return Ok(json_with_etag(headers, &page));
    }
    let nodes = all_nodes(state, now()).await?;
    let counts = group_counts(state).await?;
    let page = JobsPage {
        jobs: job_views(
            jobs,
            &nodes,
            &counts,
            &state.group_limits(),
            &state.dispatch_limits(),
            state.blocked_dependent_ttl_secs,
        ),
        next,
    };
    Ok(json_with_etag(headers, &page))
}

pub async fn get_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
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
            Ok(view) => with_version(json_with_etag(&headers, &view)),
            Err(error) => with_version(error.into_response()),
        },
        Ok(Err(e)) => with_version(store_err(e).into_response()),
        Err(_) => with_version(ApiError::internal().into_response()),
    }
}

/// Publishes a finished job's diff to its configured target. Operator-only: intake payloads can
/// never call this route, and the source-branch path rechecks the origin and approval flags.
pub async fn publish_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(error) => return with_version(error.into_response()),
    };
    let operation = "publish";
    let Ok(job_id) = JobId::parse(&id) else {
        return with_version(ApiError::not_found("not found").into_response());
    };
    let key = match request_key(&headers) {
        Ok(key) => key,
        Err(error) => return with_version(error.into_response()),
    };
    if let Err(error) = job_if_match_guard(&state, &headers, &job_id, operation, &operator).await {
        return with_version(error.into_response());
    }
    let fingerprint = key_fingerprint(operation, &[]);
    if let Some(idempotency_key) = &key {
        match claim_request_key(&state, &operator, idempotency_key, operation, &fingerprint).await {
            Ok(Some(stored)) => {
                state.record_audit(
                    Event::MutationReplay,
                    Some(job_id.as_str()),
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "replayed"),
                );
                return with_version(replayed_response(&stored));
            }
            Ok(None) => {}
            Err(error) => return with_version(error.into_response()),
        }
    }
    let rec = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.get_job(&job_id)
    })
    .await
    {
        Ok(Ok(rec)) => rec,
        Ok(Err(e)) => {
            if let Some(k) = &key {
                release_request_key(&state, &operator, k).await;
            }
            return with_version(store_err(e).into_response());
        }
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    if !rec.state.is_terminal() {
        if let Some(k) = &key {
            release_request_key(&state, &operator, k).await;
        }
        return with_version(ApiError::conflict("job is not finished").into_response());
    }
    let record = match crate::publish::publish(Arc::clone(&state), rec.clone()).await {
        Ok(record) => record,
        Err(error) => {
            let response = match error {
                crate::publish::PublishError::NotConfigured => {
                    ApiError::unprocessable("publishing is not configured")
                }
                crate::publish::PublishError::NoSpec => {
                    ApiError::unprocessable("job has no publish step")
                }
                crate::publish::PublishError::Denied(message) => ApiError::forbidden(message),
                crate::publish::PublishError::Invalid(message)
                | crate::publish::PublishError::State(message)
                | crate::publish::PublishError::Git(message) => ApiError::unprocessable(message),
                crate::publish::PublishError::RateLimited(retry_after) => {
                    ApiError::rate_limited(retry_after)
                }
            };
            if let Some(k) = &key {
                release_request_key(&state, &operator, k).await;
            }
            return with_version(response.into_response());
        }
    };
    state.record_audit(
        Event::Publish,
        Some(rec.id.as_str()),
        None,
        &operator,
        outcome_data(&state, &operator, operation, "applied")
            .text(Field::Target, record.target.as_str()),
    );
    match job_view_for(&state, &rec).await {
        Ok(mut view) => {
            view.publish = Some(record);
            let value = serde_json::to_value(&view).unwrap_or(Value::Null);
            if let Some(k) = &key {
                complete_request_key(&state, &operator, k, StatusCode::OK, &value).await;
            }
            with_version(json_ok(StatusCode::OK, &view))
        }
        Err(error) => with_version(error.into_response()),
    }
}

/// Requeues a terminal job in place from the supplied spec. With `chain=1`, blocked dependents are
/// restored transitively after the root keeps its id.
pub async fn retry_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
    req: axum::extract::Request,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(o) => o,
        Err(e) => return with_version(e.into_response()),
    };
    let operation = "retry";
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let dry_run = match dry_run_requested(&uri) {
        Ok(value) => value,
        Err(error) => return with_version(error.into_response()),
    };
    let key = match request_key(&headers) {
        Ok(key) => key,
        Err(error) => return with_version(error.into_response()),
    };
    let bytes = match read_raw_limited(req, MAX_JSON_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => return with_version(error.into_response()),
    };
    let chain = query_flag(&parse_query(&uri), "chain");
    let store = Arc::clone(&state.store);
    let id = job_id.clone();
    let current = match tokio::task::spawn_blocking(move || store.get_job(&id)).await {
        Ok(Ok(rec)) => rec,
        Ok(Err(error)) => return with_version(store_err(error).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    // With no body, reuse the stored spec so a retry needs only the job id.
    let spec = if bytes.is_empty() {
        current.spec.clone()
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(spec) => spec,
            Err(error) => {
                return with_version(ApiError::unprocessable(error.to_string()).into_response())
            }
        }
    };
    if let Err(error) = spec.validate() {
        return with_version(ApiError::unprocessable(error.to_string()).into_response());
    }
    if state.auth.restricts_fields(&operator) {
        if let Some(field) = restricted_field(&spec) {
            return with_version(
                ApiError::forbidden(format!("this token may not set {field}")).into_response(),
            );
        }
    }
    let guard = match job_if_match_guard(&state, &headers, &job_id, operation, &operator).await {
        Ok(guard) => guard,
        Err(error) => return with_version(error.into_response()),
    };
    if dry_run {
        if !current.state.is_retryable() {
            return with_version(
                ApiError::conflict(format!(
                    "job is {}; retry requires failed, cancelled, expired or deadline_missed",
                    current.state.as_str()
                ))
                .into_response(),
            );
        }
        state.record_audit(
            Event::MutationDryRun,
            Some(job_id.as_str()),
            None,
            &operator,
            outcome_data(&state, &operator, operation, "dry_run"),
        );
        return with_version(json_ok(
            StatusCode::OK,
            &json!({ "dry_run": true, "operation": operation, "job": job_id.to_string() }),
        ));
    }
    let mut fingerprint_input = bytes.to_vec();
    fingerprint_input.extend_from_slice(format!("\nchain={chain}").as_bytes());
    let fingerprint = key_fingerprint(operation, &fingerprint_input);
    if let Some(idempotency_key) = &key {
        match claim_request_key(&state, &operator, idempotency_key, operation, &fingerprint).await {
            Ok(Some(stored)) => {
                state.record_audit(
                    Event::MutationReplay,
                    Some(job_id.as_str()),
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "replayed"),
                );
                return with_version(replayed_response(&stored));
            }
            Ok(None) => {}
            Err(error) => return with_version(error.into_response()),
        }
    }
    let n = now();
    let retried = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let id = job_id.clone();
        let spec = spec.clone();
        let expected = guard.map(|g| g.updated_at);
        move || store.retry_with_dependents(&id, &spec, chain, n, expected)
    })
    .await;
    let (rec, requeued) = match retried {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            if let Some(idempotency_key) = &key {
                let store = Arc::clone(&state.store);
                let operator = operator.clone();
                let idempotency_key = idempotency_key.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    store.release_request_key(&operator, &idempotency_key)
                })
                .await;
            }
            if matches!(error, lu_store::StoreError::Precondition) {
                state.record_audit(
                    Event::MutationPreconditionFailed,
                    Some(job_id.as_str()),
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "precondition_failed"),
                );
            }
            return with_version(store_err(error).into_response());
        }
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let waiting_dependents = {
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        match tokio::task::spawn_blocking(move || store.job_counts(n, lease_ms)).await {
            Ok(Ok(counts)) => counts.waiting_dependents,
            _ => requeued.len() as u64,
        }
    };
    let requeued_ids: Vec<String> = requeued.iter().map(|id| id.to_string()).collect();
    state.record_audit(
        Event::Retried,
        Some(rec.id.as_str()),
        None,
        &operator,
        outcome_data(&state, &operator, operation, "applied")
            .unsigned(Field::Count, requeued.len() as u64)
            .text(Field::Source, if chain { "chain" } else { "job" }),
    );
    state.waiters.notify_job(rec.id.as_ref());
    for id in &requeued {
        state.record_audit(
            Event::Retried,
            Some(id.as_str()),
            None,
            &operator,
            Data::new().text(Field::Source, "dependent"),
        );
        state.waiters.notify_job(id.as_ref());
    }
    state.waiters.notify_work();
    let response = json!({
        "job": job_view_with_ttl(&rec, state.blocked_dependent_ttl_secs),
        "requeued": requeued_ids,
        "waiting_dependents": waiting_dependents,
    });
    if let Some(idempotency_key) = &key {
        complete_request_key(
            &state,
            &operator,
            idempotency_key,
            StatusCode::OK,
            &response,
        )
        .await;
    }
    with_version(json_ok(StatusCode::OK, &response))
}

pub async fn cancel_job(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(o) => o,
        Err(e) => return with_version(e.into_response()),
    };
    let operation = "cancel";
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let dry_run = match dry_run_requested(&uri) {
        Ok(value) => value,
        Err(error) => return with_version(error.into_response()),
    };
    let key = match request_key(&headers) {
        Ok(key) => key,
        Err(error) => return with_version(error.into_response()),
    };
    // Both flags are operator escape hatches. By default a cancellation clears the spawn tree and
    // the blocked dependents that can no longer run after their predecessor dies.
    let query = crate::routes::parse_query(&uri);
    let keep_children = query_flag(&query, "keep_children");
    let keep_dependents = query_flag(&query, "keep_dependents");
    let guard = match job_if_match_guard(&state, &headers, &job_id, operation, &operator).await {
        Ok(guard) => guard,
        Err(error) => return with_version(error.into_response()),
    };
    if dry_run {
        state.record_audit(
            Event::MutationDryRun,
            Some(job_id.as_str()),
            None,
            &operator,
            outcome_data(&state, &operator, operation, "dry_run"),
        );
        return with_version(json_ok(
            StatusCode::OK,
            &json!({ "dry_run": true, "operation": operation, "job": job_id.to_string() }),
        ));
    }
    let fingerprint = key_fingerprint(operation, uri.query().unwrap_or("").as_bytes());
    if let Some(idempotency_key) = &key {
        match claim_request_key(&state, &operator, idempotency_key, operation, &fingerprint).await {
            Ok(Some(stored)) => {
                state.record_audit(
                    Event::MutationReplay,
                    Some(job_id.as_str()),
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "replayed"),
                );
                return with_version(replayed_response(&stored));
            }
            Ok(None) => {}
            Err(error) => return with_version(error.into_response()),
        }
    }
    let expected = guard.map(|g| g.updated_at);
    let changed = match crate::routes::cancel_job_with_descendants(
        &state,
        &job_id,
        keep_children,
        keep_dependents,
        expected,
    )
    .await
    {
        Ok(ids) => ids,
        Err(error) => {
            if let Some(idempotency_key) = &key {
                let store = Arc::clone(&state.store);
                let operator = operator.clone();
                let idempotency_key = idempotency_key.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    store.release_request_key(&operator, &idempotency_key)
                })
                .await;
            }
            if error.code == crate::error::ErrorCode::Precondition {
                state.record_audit(
                    Event::MutationPreconditionFailed,
                    Some(job_id.as_str()),
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "precondition_failed"),
                );
            }
            return with_version(error.into_response());
        }
    };
    if !changed.contains(&job_id) {
        let res = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            move || store.get_job(&job_id).map(|_| ()).map_err(|_| ())
        })
        .await;
        if !matches!(res, Ok(Ok(()))) {
            return with_version(ApiError::not_found("not found").into_response());
        }
        return with_version(ApiError::conflict("job is already terminal").into_response());
    }
    for changed_id in &changed {
        let data = outcome_data(&state, &operator, operation, "applied");
        state.record_audit(
            Event::Cancelled,
            Some(changed_id.as_str()),
            None,
            &operator,
            if changed_id == &job_id {
                data
            } else {
                Data::new().text(Field::Source, "dependent")
            },
        );
        state.waiters.notify_job(changed_id.as_ref());
    }
    state.waiters.notify_work();
    let rec = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.get_job(&job_id)
    })
    .await;
    match rec {
        Ok(Ok(rec)) => match job_view_for(&state, &rec).await {
            Ok(view) => {
                let value = serde_json::to_value(&view).unwrap_or(Value::Null);
                if let Some(idempotency_key) = &key {
                    complete_request_key(
                        &state,
                        &operator,
                        idempotency_key,
                        StatusCode::OK,
                        &value,
                    )
                    .await;
                }
                with_version(json_ok(StatusCode::OK, &view))
            }
            Err(error) => with_version(error.into_response()),
        },
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
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(_) => return with_version(ApiError::unauthorized().into_response()),
    };
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    if let Err(error) = job_if_match_guard(&state, &headers, &job_id, "edit", &operator).await {
        return with_version(error.into_response());
    }
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
            state.record_audit(
                Event::JobEdited,
                Some(rec.id.as_str()),
                None,
                &operator,
                outcome_data(&state, &operator, "edit", "applied")
                    .text(Field::Source, role_name(&state, &operator)),
            );
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

#[derive(Deserialize, Default)]
struct ApproveBody {
    /// Optional edit applied before approval, for `lu approve --edit`.
    edit: Option<JobEdit>,
}

#[derive(Deserialize, Default)]
struct RejectBody {
    /// Recorded only as a safe reason code in the audit event; free text is never stored.
    reason: Option<String>,
}

/// Operator-only draft inventory, oldest first. Drafts are never dispatched from this route.
pub async fn list_drafts(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
    }
    let q = parse_query(&uri);
    let limit = match query_u64(&q, "limit") {
        Ok(value) => usize::try_from(value.unwrap_or(200))
            .unwrap_or(usize::MAX)
            .min(1000),
        Err(error) => return with_version(error.into_response()),
    };
    let store = Arc::clone(&state.store);
    let drafts = match tokio::task::spawn_blocking(move || store.list_drafts(limit)).await {
        Ok(Ok(jobs)) => jobs,
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
    let next = drafts.last().map(|job| job.id.to_string());
    with_version(json_value_ok(
        StatusCode::OK,
        json!({
            "drafts": job_views(
                &drafts,
                &nodes,
                &counts,
                &state.group_limits(),
                &state.dispatch_limits(),
                state.blocked_dependent_ttl_secs,
            ),
            "next": next,
        }),
    ))
}

/// Approves a draft. With a body containing `edit`, the edit is applied first; the job stays a
/// draft until the transition succeeds.
pub async fn approve_draft(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(_) => return with_version(ApiError::unauthorized().into_response()),
    };
    let operation = "approve";
    let Ok(job_id) = JobId::parse(&id) else {
        return with_version(ApiError::not_found("not found").into_response());
    };
    let key = match request_key(&headers) {
        Ok(key) => key,
        Err(error) => return with_version(error.into_response()),
    };
    let bytes = match read_raw_limited(req, MAX_JSON_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => return with_version(error.into_response()),
    };
    let edit: Option<JobEdit> = if bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice::<ApproveBody>(&bytes) {
            Ok(body) => body.edit,
            Err(_) => {
                return with_version(ApiError::bad_request("invalid approval body").into_response())
            }
        }
    };
    let guard = match job_if_match_guard(&state, &headers, &job_id, operation, &operator).await {
        Ok(guard) => guard,
        Err(error) => return with_version(error.into_response()),
    };
    let fingerprint = key_fingerprint(operation, &bytes);
    if let Some(idempotency_key) = &key {
        match claim_request_key(&state, &operator, idempotency_key, operation, &fingerprint).await {
            Ok(Some(stored)) => {
                state.record_audit(
                    Event::MutationReplay,
                    Some(job_id.as_str()),
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "replayed"),
                );
                return with_version(replayed_response(&stored));
            }
            Ok(None) => {}
            Err(error) => return with_version(error.into_response()),
        }
    }
    let has_dependencies = match guard {
        Some(g) => g.has_dependencies,
        None => {
            let store = Arc::clone(&state.store);
            let id = job_id.clone();
            match tokio::task::spawn_blocking(move || store.get_job(&id)).await {
                Ok(Ok(rec)) => !rec.waiting_on.is_empty(),
                _ => false,
            }
        }
    };
    let target = if has_dependencies {
        lu_proto::JobState::Blocked
    } else {
        lu_proto::JobState::Queued
    };
    let store = Arc::clone(&state.store);
    let id_for_write = job_id.clone();
    let expected = guard.map(|g| g.updated_at);
    let result = tokio::task::spawn_blocking(move || {
        store.approve_draft_if(&id_for_write, edit.as_ref(), target, now(), expected)
    })
    .await;
    let rec = match result {
        Ok(Ok(rec)) => rec,
        Ok(Err(e)) => {
            if let Some(idempotency_key) = &key {
                let store = Arc::clone(&state.store);
                let operator = operator.clone();
                let idempotency_key = idempotency_key.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    store.release_request_key(&operator, &idempotency_key)
                })
                .await;
            }
            if matches!(e, lu_store::StoreError::Precondition) {
                state.record_audit(
                    Event::MutationPreconditionFailed,
                    Some(job_id.as_str()),
                    None,
                    &operator,
                    outcome_data(&state, &operator, operation, "precondition_failed"),
                );
            }
            return with_version(store_err(e).into_response());
        }
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    // Only an explicit operator approval may enable a source-branch push for an intake job.
    // The approval is process-local and never written into the job spec.
    state.mark_approved(&rec.id);
    if rec.state == lu_proto::JobState::Queued {
        state.waiters.notify_work();
    }
    state.record_audit(
        Event::DraftApproved,
        Some(rec.id.as_str()),
        None,
        &operator,
        outcome_data(&state, &operator, operation, "applied")
            .text(Field::State, rec.state.as_str()),
    );
    match job_view_for(&state, &rec).await {
        Ok(view) => {
            let value = serde_json::to_value(&view).unwrap_or(Value::Null);
            if let Some(idempotency_key) = &key {
                complete_request_key(&state, &operator, idempotency_key, StatusCode::OK, &value)
                    .await;
            }
            with_version(json_ok(StatusCode::OK, &view))
        }
        Err(error) => with_version(error.into_response()),
    }
}

/// Rejects a draft into terminal `cancelled`. The reason is a safe audit code, never job text.
pub async fn reject_draft(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(_) => return with_version(ApiError::unauthorized().into_response()),
    };
    let Ok(job_id) = JobId::parse(&id) else {
        return with_version(ApiError::not_found("not found").into_response());
    };
    if let Err(error) = job_if_match_guard(&state, &headers, &job_id, "reject", &operator).await {
        return with_version(error.into_response());
    }
    let bytes = match read_raw_limited(req, MAX_JSON_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => return with_version(error.into_response()),
    };
    let reason = if bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice::<RejectBody>(&bytes) {
            Ok(body) => body.reason,
            Err(_) => {
                return with_version(
                    ApiError::bad_request("invalid rejection body").into_response(),
                )
            }
        }
    };
    let store = Arc::clone(&state.store);
    let result = tokio::task::spawn_blocking(move || {
        let n = now();
        let rec = store.get_job(&job_id)?;
        if rec.state != lu_proto::JobState::Draft {
            return Err(lu_store::StoreError::Conflict { length: None });
        }
        store.cancel_job(&job_id, n)
    })
    .await;
    let rec = match result {
        Ok(Ok(rec)) => rec,
        Ok(Err(e)) => return with_version(store_err(e).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let code = match reason.as_deref() {
        Some("duplicate") => "duplicate",
        Some("spam") => "spam",
        Some("unsafe") => "unsafe",
        _ => "rejected",
    };
    state.record_audit(
        Event::DraftRejected,
        Some(rec.id.as_str()),
        None,
        &operator,
        outcome_data(&state, &operator, "reject", "applied").text(Field::Mode, code),
    );
    match job_view_for(&state, &rec).await {
        Ok(view) => with_version(json_ok(StatusCode::OK, &view)),
        Err(error) => with_version(error.into_response()),
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
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
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
    let ct = if matches!(art, ArtifactName::ReportJson) {
        "application/json"
    } else {
        "application/octet-stream"
    };
    let result = async {
        let len = file_len(&path, art.max_bytes()).await?;
        stream_range(&path, art.max_bytes(), 0, len).await
    }
    .await;
    match result {
        Ok(file) => with_version(into_response(file, ct, art.file_name())),
        Err(error) => with_version(error.into_response()),
    }
}

pub async fn get_logs(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
    }
    let job_id = match JobId::parse(&id) {
        Ok(i) => i,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let q = crate::routes::parse_query(&uri);
    let offset = match query_u64(&q, "offset") {
        Ok(value) => value.unwrap_or(0),
        Err(error) => return with_version(error.into_response()),
    };
    // `max` bounds one response. Absent means the log cap; larger values are clamped to it.
    let max = match query_u64(&q, "max") {
        Ok(None) => lu_proto::MAX_LOG_BYTES,
        Ok(Some(0)) => return with_version(ApiError::bad_request("invalid max").into_response()),
        Ok(Some(value)) => value.min(lu_proto::MAX_LOG_BYTES),
        Err(error) => return with_version(error.into_response()),
    };
    let wait_secs = match query_u64(&q, "wait") {
        Ok(value) => value.unwrap_or(0).min(25),
        Err(error) => return with_version(error.into_response()),
    };
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
    let requested = rec.log_len.saturating_sub(offset).min(max);
    let (body, len) = if requested == 0 {
        (axum::body::Body::empty(), 0)
    } else {
        match stream_range(&path, lu_proto::MAX_LOG_BYTES, offset, requested).await {
            Ok(file) => (file.body, file.len),
            Err(error) => return with_version(error.into_response()),
        }
    };
    let res = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"run.log\"",
        )
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-lu-log-length", rec.log_len.to_string())
        .header("x-lu-state", rec.state.as_str())
        .body(body)
        .expect("valid log response headers");
    with_version(res)
}

pub async fn list_nodes(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
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
            let node_views = nodes
                .iter()
                .map(|node| {
                    let details = cordons
                        .iter()
                        .zip(views.iter())
                        .filter(|(cordon, _)| cordon.node == node.name)
                        .map(|(_, view)| view.clone())
                        .collect();
                    node_view(
                        node,
                        stats.get(&node.name),
                        details,
                        state.dispatch_limits().min_disk_free_pct,
                    )
                })
                .collect::<Vec<_>>();
            with_version(json_with_etag(&headers, &node_views))
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
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
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
        Some(stats) => with_version(json_with_etag(&headers, &stats)),
        None => with_version(json_with_etag(&headers, &json!({}))),
    }
}

/// Queues a workspace-clean event for a node. Deletion happens on the agent's next poll, and
/// running jobs are always refused there.
pub async fn clean_workspaces(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
    uri: Uri,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(_) => return with_version(ApiError::unauthorized().into_response()),
    };
    let operation = "clean";
    let dry_run = match dry_run_requested(&uri) {
        Ok(value) => value,
        Err(error) => return with_version(error.into_response()),
    };
    if let Err(error) = star_if_match(&state, &headers, operation, &operator) {
        return with_version(error.into_response());
    }
    let node = match NodeName::parse(&name) {
        Ok(n) => n,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let query = parse_query(&uri);
    for key in query.keys() {
        if key != "all" && key != "job" && key != "dry_run" {
            return with_version(ApiError::bad_request("unknown query parameter").into_response());
        }
    }
    let all = match query.get("all").map(String::as_str) {
        None | Some("false") | Some("0") => false,
        Some("true") | Some("1") => true,
        Some(_) => return with_version(ApiError::bad_request("invalid all").into_response()),
    };
    let job = match query.get("job") {
        Some(value) => match JobId::parse(value) {
            Ok(id) => Some(id),
            Err(_) => return with_version(ApiError::bad_request("invalid job").into_response()),
        },
        None => None,
    };
    if all == job.is_some() {
        return with_version(
            ApiError::bad_request("pass either all=true or job=<key>").into_response(),
        );
    }
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
    if dry_run {
        state.record_audit(
            Event::MutationDryRun,
            None,
            Some(node.as_str()),
            &operator,
            outcome_data(&state, &operator, operation, "dry_run"),
        );
        return with_version(json_ok(
            StatusCode::OK,
            &json!({
                "dry_run": true,
                "operation": operation,
                "node": node.as_str(),
                "queued": 1,
                "refused_running": 0,
                "missing": 0
            }),
        ));
    }
    state.record_audit(
        Event::WorkspaceClean,
        None,
        Some(node.as_str()),
        &operator,
        outcome_data(&state, &operator, operation, "applied")
            .text(Field::Source, role_name(&state, &operator)),
    );
    let mut event = serde_json::json!({ "kind": "clean_workspaces", "all": all });
    if let Some(job) = job {
        event["job"] = serde_json::json!(job.to_string());
    }
    state
        .pending_clean
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(node)
        .or_default()
        .push(event);
    with_version(json_ok(
        StatusCode::OK,
        &WorkspaceCleanResult {
            queued: 1,
            refused_running: 0,
            missing: 0,
        },
    ))
}

pub async fn list_repos(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
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
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(_) => return with_version(ApiError::unauthorized().into_response()),
    };
    if let Err(error) = star_if_match(&state, &headers, "repo_sync", &operator) {
        return with_version(error.into_response());
    }
    if state.mirror.repo(&name).is_none() {
        return with_version(ApiError::not_found("not found").into_response());
    }
    // A sync runs git and touches disk. Keep one in flight and a small per-operator budget;
    // contention is a retryable 429 rather than queued blocking work.
    let Ok(slot) = Arc::clone(&state.sync_slots).try_acquire_owned() else {
        return with_version(ApiError::rate_limited(1).into_response());
    };
    if let Err(retry_after) = state.sync_limiter.allow(&operator, SYNC_PER_TOKEN) {
        drop(slot);
        return with_version(ApiError::rate_limited(retry_after).into_response());
    }
    let mirror = Arc::clone(&state.mirror);
    let sync_name = name.clone();
    let log_name = name.clone();
    let result = tokio::task::spawn_blocking(move || mirror.sync(&sync_name)).await;
    match result {
        Ok(Ok(sync)) => {
            state.record_audit(
                Event::RepoSync,
                None,
                None,
                &operator,
                outcome_data(&state, &operator, "repo_sync", "applied")
                    .text(Field::Source, role_name(&state, &operator))
                    .text(Field::Remote, &log_name),
            );
            with_version(json_ok(StatusCode::OK, &sync))
        }
        Ok(Err(e)) => {
            tracing::error!(repo = %log_name, error = %e, "repository sync failed");
            with_version(ApiError::internal().into_response())
        }
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
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(o) => o,
        Err(e) => return with_version(e.into_response()),
    };
    if let Err(error) = star_if_match(
        &state,
        &headers,
        if draining { "drain" } else { "undrain" },
        &operator,
    ) {
        return with_version(error.into_response());
    }
    let node = match NodeName::parse(&name) {
        Ok(n) => n,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let audit_node = node.as_str().to_owned();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.set_draining(&node, draining)
    })
    .await;
    match res {
        Ok(Ok(())) => {
            state.record_audit(
                Event::Drain,
                None,
                Some(&audit_node),
                &operator,
                outcome_data(
                    &state,
                    &operator,
                    if draining { "drain" } else { "undrain" },
                    "applied",
                )
                .bool(Field::State, draining),
            );
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
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(o) => o,
        Err(e) => return with_version(e.into_response()),
    };
    if let Err(error) = star_if_match(&state, &headers, "uncordon", &operator) {
        return with_version(error.into_response());
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
    let audit_node = node.as_str().to_owned();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.uncordon(&node, provider.as_deref())
    })
    .await;
    match res {
        Ok(Ok(removed)) => {
            for provider in &removed {
                state.record_audit(
                    Event::CordonCleared,
                    None,
                    Some(&audit_node),
                    &operator,
                    outcome_data(&state, &operator, "uncordon", "applied")
                        .text(Field::Provider, provider),
                );
            }
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
                allow_empty,
                requires: Vec::new(),
                tolerates: Vec::new(),
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
                schedule: None,
            },
            attempt: 1,
            node: None,
            assigned_provider: None,
            assigned_model: None,
            epoch: 1,
            created_at: 1,
            updated_at: 2,
            eligible_since: 1,
            started_at: None,
            finished_at: None,
            failure_class: None,
            failure_reason: None,
            excluded_nodes: Vec::new(),
            summary: None,
            exit_code: Some(0),
            artifacts,
            cancel_requested: false,
            log_len: 0,
            diff_bytes,
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
        }
    }

    fn diff() -> Vec<ArtifactName> {
        vec![ArtifactName::DiffPatch]
    }

    #[test]
    fn a_stored_repo_with_userinfo_is_redacted_in_views() {
        let mut r = record(JobState::Queued, false, None, Vec::new());
        let user = ['v', 'i', 'e', 'w'].iter().collect::<String>();
        let pass = ['o', 'n', 'l', 'y'].iter().collect::<String>();
        let raw = format!("https://{user}:{pass}@example.net/repo.git?ref=main#part");
        r.spec.repo = raw.clone();
        r.spec.source.repo = raw;
        let view = job_view(&r);
        assert_eq!(view.spec.source.repo, "https://example.net/repo.git");
        assert_eq!(view.spec.repo, "https://example.net/repo.git");
        let wire = serde_json::to_string(&view).expect("serialize view");
        assert!(!wire.contains(&user));
        assert!(!wire.contains(&pass));
        assert!(!wire.contains("ref=main"));
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
