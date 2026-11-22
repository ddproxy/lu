use crate::error::ApiError;
use crate::intake::{to_spec, Decision};
use crate::routes::{json_value_ok, store_err};
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_audit::{Data, Event, Field};
use serde_json::Value;
use std::sync::Arc;

/// A signed delivery is never echoed: every failure below has the same shape and carries no
/// payload text, header value or signature detail.
fn unauthorized() -> Response {
    let mut response = ApiError::unauthorized().into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
    response
}

fn outcome(state: &AppState, source: &str, decision: Decision) -> Response {
    state.record_intake(source, decision.label());
    match decision {
        Decision::Accepted { .. } => StatusCode::ACCEPTED.into_response(),
        Decision::Dropped | Decision::Duplicate => StatusCode::NO_CONTENT.into_response(),
        Decision::BadSignature => unauthorized(),
        Decision::RateLimited => ApiError::rate_limited(60).into_response(),
        Decision::Invalid => ApiError::bad_request("invalid delivery").into_response(),
    }
}

pub async fn intake(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(intake) = state.intake.as_ref() else {
        return ApiError::not_found("not found").into_response();
    };
    if !intake.has_source(&name) {
        return ApiError::not_found("not found").into_response();
    }
    let source = intake.source_label(&name);
    let signature = intake
        .signature_header(&name)
        .and_then(|header| headers.get(header))
        .and_then(|value| value.to_str().ok());
    // Verify the raw bytes before even checking the size or parsing JSON. Bad and missing
    // signatures are indistinguishable to the caller.
    if !intake.verify(&name, signature, &body) {
        return outcome(&state, source, Decision::BadSignature);
    }
    if body.len() > intake.max_body_bytes(&name) {
        return outcome(&state, source, Decision::Invalid);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return outcome(&state, source, Decision::Invalid);
    };
    let delivery_id = intake
        .dedupe_header(&name)
        .and_then(|header| headers.get(header))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match intake.admit(&name, delivery_id) {
        Decision::Accepted { .. } => {}
        other => return outcome(&state, source, other),
    }
    let job = match intake.map(&name, &value) {
        Ok(Some(job)) => job,
        Ok(None) => return outcome(&state, source, Decision::Dropped),
        Err(_) => return outcome(&state, source, Decision::Invalid),
    };
    let mut spec = to_spec(&job, source, delivery_id);
    // The controller resolves the named ref (including PR refs) and pins the commit itself, so
    // a node never needs forge access and cannot reinterpret a mutable branch name.
    if let Some(repo) = state.mirror.repo_for_url(&spec.source.repo) {
        let repo_name = repo.name.clone();
        let git_ref = spec.source.git_ref.clone();
        let mirror = Arc::clone(&state.mirror);
        let pinned =
            tokio::task::spawn_blocking(move || mirror.ensure_resolved(&repo_name, &git_ref)).await;
        let commit = match pinned {
            Ok(Ok(commit)) => commit,
            _ => {
                intake.rollback(&name, delivery_id);
                return outcome(&state, source, Decision::Invalid);
            }
        };
        spec.base_commit = Some(commit.clone());
        spec.source.base = Some(commit);
    }
    if let Err(error) = spec.validate() {
        tracing::warn!(
            source,
            field = error.field(),
            "intake rule produced an invalid spec"
        );
        intake.rollback(&name, delivery_id);
        return outcome(&state, source, Decision::Invalid);
    }

    if let Err(error) = crate::routes::enforce_draft_capacity(&state, &spec).await {
        intake.rollback(&name, delivery_id);
        state.record_intake(source, "invalid");
        return error.into_response();
    }

    let is_draft = spec.draft;
    let store = Arc::clone(&state.store);
    let created =
        tokio::task::spawn_blocking(move || store.create_job(&spec, crate::time::now())).await;
    let (record, _new) = match created {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            intake.rollback(&name, delivery_id);
            return store_err(error).into_response();
        }
        Err(_) => {
            intake.rollback(&name, delivery_id);
            return ApiError::internal().into_response();
        }
    };
    if !is_draft {
        state.waiters.notify_work();
    }
    state.record_audit(
        Event::Submitted,
        Some(record.id.as_str()),
        None,
        source,
        Data::new().text(Field::Source, source),
    );
    state.record_intake(source, "accepted");
    let body = serde_json::json!({ "id": record.id.as_str(), "state": record.state.as_str() });
    let mut response = json_value_ok(StatusCode::ACCEPTED, body);
    response.headers_mut().insert(
        header::LOCATION,
        format!("/v1/jobs/{}", record.id).parse().unwrap(),
    );
    response
}
