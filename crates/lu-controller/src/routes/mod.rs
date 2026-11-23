pub mod agent;
pub mod file_body;
pub mod health;
pub mod intake;
pub mod metrics;
pub mod operator;
pub mod schedule;
pub mod scope;
pub mod stats;
pub mod status;

use crate::error::{ApiError, ApiResult};
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

pub const MAX_JSON_BODY: usize = 1024 * 1024;
pub const MAX_LOG_CHUNK_BODY: usize = lu_proto::MAX_LOG_CHUNK_BYTES + 1024; // small slack
pub const MAX_DIFF_BODY: usize = (10 * 1024 * 1024) + 4096;
pub const MAX_REPORT_BODY: usize = (1024 * 1024) + 4096;

pub fn json_ok<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_string(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(_) => ApiError::internal().into_response(),
    }
}

pub fn json_value_ok(status: StatusCode, value: serde_json::Value) -> Response {
    json_ok(status, &value)
}

pub async fn read_json<T: DeserializeOwned>(req: Request, limit: usize) -> ApiResult<T> {
    // Enforce content-type? not specified, but strict JSON parsing required.
    let bytes = to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| ApiError::too_large("request body too large"))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::bad_request("invalid request body"))
}

pub async fn read_raw_limited(req: Request, limit: usize) -> ApiResult<axum::body::Bytes> {
    to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| ApiError::too_large("request body too large"))
}

/// Applies `[drafts]` capacity to a new draft, expiring the oldest drafts first. The caller must
/// call this before writing; a full store returns a typed `limit_exceeded` error. Per-source cap
/// overflow only evicts drafts from that origin; global overflow evicts oldest drafts overall.
pub async fn enforce_draft_capacity(
    state: &std::sync::Arc<crate::state::AppState>,
    spec: &lu_proto::JobSpec,
) -> Result<(), ApiError> {
    if !spec.draft {
        return Ok(());
    }
    let policy = state.drafts;
    let origin = spec.origin.clone().unwrap_or_default();
    let counts = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.draft_counts()
    })
    .await;
    let mut counts = match counts {
        Ok(Ok(counts)) => counts,
        _ => return Err(ApiError::internal()),
    };
    let mut per_source = counts
        .by_origin
        .iter()
        .find(|(label, _)| label == &origin)
        .map(|(_, count)| *count)
        .unwrap_or(0);
    let per_source_overflow = per_source
        .saturating_add(1)
        .saturating_sub(policy.max_per_source);
    if per_source_overflow > 0 {
        let expired = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            let now = crate::time::now();
            let origin = origin.clone();
            move || store.expire_oldest_drafts(per_source_overflow, now, Some(&origin))
        })
        .await;
        if !matches!(expired, Ok(Ok(_))) {
            return Err(ApiError::internal());
        }
        counts = match tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            move || store.draft_counts()
        })
        .await
        {
            Ok(Ok(counts)) => counts,
            _ => return Err(ApiError::internal()),
        };
        per_source = counts
            .by_origin
            .iter()
            .find(|(label, _)| label == &origin)
            .map(|(_, count)| *count)
            .unwrap_or(0);
    }
    let total_overflow = counts
        .total
        .saturating_add(1)
        .saturating_sub(policy.max_total);
    if total_overflow > 0 {
        let expired = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            let now = crate::time::now();
            move || store.expire_oldest_drafts(total_overflow, now, None)
        })
        .await;
        if !matches!(expired, Ok(Ok(_))) {
            return Err(ApiError::internal());
        }
        counts = match tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            move || store.draft_counts()
        })
        .await
        {
            Ok(Ok(counts)) => counts,
            _ => return Err(ApiError::internal()),
        };
        per_source = counts
            .by_origin
            .iter()
            .find(|(label, _)| label == &origin)
            .map(|(_, count)| *count)
            .unwrap_or(0);
    }
    if counts.total >= policy.max_total || per_source >= policy.max_per_source {
        return Err(ApiError::limit_exceeded("max_drafts"));
    }
    Ok(())
}

pub fn store_err(e: lu_store::StoreError) -> ApiError {
    use lu_store::StoreError;
    match e {
        StoreError::NotFound => ApiError::not_found("not found"),
        StoreError::Conflict { length } => {
            let mut err = ApiError::conflict("conflict");
            if let Some(l) = length {
                err = err.with_log_length(l);
            }
            err
        }
        StoreError::StaleEpoch => ApiError::stale_epoch(),
        StoreError::InvalidDependency(id) => {
            ApiError::unprocessable(format!("unknown dependency {id}"))
        }
        StoreError::DeadDependency(id) => {
            ApiError::unprocessable(format!("dependency {id} already failed or was cancelled"))
        }
        StoreError::DependencyCycle => ApiError::bad_request("dependencies would form a cycle"),
        StoreError::InvalidSpec(message) => ApiError::unprocessable(message),
        StoreError::LimitExceeded(limit) => ApiError::limit_exceeded(limit),
        StoreError::Internal(_) => ApiError::internal(),
    }
}

/// Cancels one job and, by default, every descendant and blocked dependent. A terminal job is
/// left as it is; a running job receives the same cancel request as its predecessor. `keep_children`
/// and `keep_dependents` are the two operator escape hatches.
pub async fn cancel_job_with_descendants(
    state: &std::sync::Arc<crate::state::AppState>,
    id: &lu_proto::JobId,
    keep_children: bool,
    keep_dependents: bool,
) -> Result<Vec<lu_proto::JobId>, ApiError> {
    let store = std::sync::Arc::clone(&state.store);
    let mut targets = vec![id.clone()];
    if !keep_children {
        let scan_store = std::sync::Arc::clone(&store);
        let id_for_scan = id.clone();
        let descendants = tokio::task::spawn_blocking(move || {
            fn collect(
                store: &dyn lu_store::Store,
                parent: &lu_proto::JobId,
                out: &mut Vec<lu_proto::JobId>,
            ) -> Result<(), lu_store::StoreError> {
                for child in store.children(parent)? {
                    out.push(child.id.clone());
                    collect(store, &child.id, out)?;
                }
                Ok(())
            }
            let mut out = Vec::new();
            collect(scan_store.as_ref(), &id_for_scan, &mut out)?;
            Ok::<_, lu_store::StoreError>(out)
        })
        .await;
        match descendants {
            Ok(Ok(mut children)) => targets.append(&mut children),
            Ok(Err(error)) => return Err(store_err(error)),
            Err(_) => return Err(ApiError::internal()),
        }
    }
    let targets = {
        let mut seen = std::collections::HashSet::new();
        targets
            .into_iter()
            .filter(|job| seen.insert(job.clone()))
            .collect::<Vec<_>>()
    };
    // Enumerating blocked dependents before the transaction lets the caller wake and audit every
    // job the store cascade touches, while the store remains responsible for the actual cascade.
    let mut dependents = Vec::new();
    if !keep_dependents {
        let scan_store = std::sync::Arc::clone(&store);
        let scan_targets = targets.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for target in scan_targets {
                for dependent in scan_store.blocked_dependents(&target)? {
                    out.push(dependent.id);
                }
            }
            Ok::<_, lu_store::StoreError>(out)
        })
        .await;
        match found {
            Ok(Ok(ids)) => dependents = ids,
            Ok(Err(error)) => return Err(store_err(error)),
            Err(_) => return Err(ApiError::internal()),
        }
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut changed = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for target in targets {
            match store.cancel_job_with_dependents(&target, crate::time::now(), keep_dependents) {
                Ok(_) if seen.insert(target.clone()) => changed.push(target),
                Ok(_) => {}
                Err(lu_store::StoreError::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        for dependent in dependents {
            if seen.insert(dependent.clone()) {
                changed.push(dependent);
            }
        }
        Ok::<_, lu_store::StoreError>(changed)
    })
    .await;
    match result {
        Ok(Ok(changed)) => Ok(changed),
        Ok(Err(error)) => Err(store_err(error)),
        Err(_) => Err(ApiError::internal()),
    }
}

pub fn job_state_str(s: lu_proto::JobState) -> &'static str {
    s.as_str()
}

pub fn failure_class_str(class: lu_proto::FailureClass) -> &'static str {
    match class {
        lu_proto::FailureClass::Node => "node",
        lu_proto::FailureClass::Provider => "provider",
        lu_proto::FailureClass::Execution => "execution",
        lu_proto::FailureClass::Dependency => "dependency",
        lu_proto::FailureClass::Conflict => "conflict",
    }
}

pub fn version_header() -> HeaderValue {
    HeaderValue::from_static(concat!("lu/", env!("CARGO_PKG_VERSION")))
}

pub fn with_version(mut res: Response) -> Response {
    let headers = res.headers_mut();
    headers.insert("x-lu-version", version_header());
    // Also applied by the router middleware; keeping it here covers direct handler use and tests.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

/// Every value of a repeated query parameter, in order, percent-decoded. Empty pairs are skipped.
pub fn query_values(uri: &Uri, name: &str) -> Vec<String> {
    uri.query()
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then(|| percent_decode(value))
        })
        .collect()
}

pub fn parse_query(uri: &Uri) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = uri.query() {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            // minimal decoding for '+' and %xx common cases
            let v = percent_decode(v);
            out.insert(k.to_string(), v);
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
