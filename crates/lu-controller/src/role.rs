//! Role enforcement for operator tokens.
//!
//! A `viewer` token may read a fixed set of summary routes and nothing else. Task text and
//! context are removed from every JSON response it receives, so a read-only dashboard never
//! carries what operators wrote into jobs. Logs, artifacts, drafts and every mutation are refused.

use crate::error::ApiError;
use crate::routes::with_version;
use crate::state::AppState;
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{header, Method, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use lu_config::OperatorRole;
use serde_json::Value;
use std::sync::Arc;

/// Largest response body a viewer response is rewritten from.
const MAX_REWRITE_BYTES: usize = 16 * 1024 * 1024;

/// JSON keys that carry what an operator wrote into a job.
const HIDDEN_KEYS: [&str; 2] = ["task", "context"];

/// Whether a viewer may call `method` on `path`.
pub fn viewer_allows(method: &Method, path: &str) -> bool {
    if method != Method::GET && method != Method::HEAD {
        return false;
    }
    match path {
        "/v1/status" | "/v1/stats" | "/v1/jobs" | "/v1/nodes" | "/v1/repos" | "/v1/health"
        | "/metrics" => true,
        _ => {
            let mut parts = path.trim_start_matches('/').split('/');
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some("v1"), Some("jobs"), Some(_), None) => true,
                (Some("v1"), Some("nodes"), Some(_), Some("stats")) => parts.next().is_none(),
                _ => false,
            }
        }
    }
}

/// Removes task text and context everywhere in a JSON document.
pub fn strip_hidden(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for key in HIDDEN_KEYS {
                map.remove(key);
            }
            map.values_mut().for_each(strip_hidden);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_hidden),
        _ => {}
    }
}

fn bearer(req: &Request<Body>) -> Option<&str> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

pub async fn role_guard(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let viewer = bearer(&req)
        .and_then(|token| state.auth.operator_for_token(token))
        .and_then(|name| state.auth.profile(name))
        .is_some_and(|profile| profile.role == OperatorRole::Viewer);
    if !viewer {
        return next.run(req).await;
    }
    if !viewer_allows(req.method(), req.uri().path()) {
        return with_version(ApiError::forbidden("forbidden").into_response());
    }
    let response = next.run(req).await;
    let json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_REWRITE_BYTES).await else {
        return with_version(ApiError::internal().into_response());
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    strip_hidden(&mut value);
    let rewritten = serde_json::to_vec(&value).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(rewritten))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_routes_are_read_only_and_exclude_logs_and_artifacts() {
        let get = Method::GET;
        for path in [
            "/v1/status",
            "/v1/jobs",
            "/v1/jobs/01ABC",
            "/v1/nodes",
            "/v1/nodes/a/stats",
        ] {
            assert!(viewer_allows(&get, path), "{path}");
        }
        for path in [
            "/v1/jobs/01ABC/logs",
            "/v1/jobs/01ABC/artifacts/diff.patch",
            "/v1/drafts",
            "/v1/scope/x",
            "/v1/agent/self",
            "/v1/nodes/a/drain",
        ] {
            assert!(!viewer_allows(&get, path), "{path}");
        }
        for path in ["/v1/jobs", "/v1/jobs/01ABC/cancel", "/v1/nodes/a/drain"] {
            assert!(!viewer_allows(&Method::POST, path), "{path}");
        }
        assert!(!viewer_allows(&Method::PUT, "/v1/drafts/x"));
    }

    #[test]
    fn hidden_keys_are_removed_at_every_depth() {
        let mut value = serde_json::json!({
            "id": "1",
            "task": "secret",
            "spec": {"task": "secret", "context": "more", "repo": "r"},
            "jobs": [{"context": "x", "state": "done"}]
        });
        strip_hidden(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"id": "1", "spec": {"repo": "r"}, "jobs": [{"state": "done"}]})
        );
    }
}
