use crate::error::ApiError;
use crate::routes::with_version;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    // "constant-cost" and check db status? store has no ping method; best effort cheap query.
    // Use a simple call that is bounded; list_jobs with limit 0 not possible, so try nodes/list logic?
    // Alternatively treat as ok if state exists. But contract shows `"db":"ok"`.
    let db_ok = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.list_jobs(None, 1, None).is_ok()
    })
    .await
    .unwrap_or(false);
    if !db_ok {
        return with_version(ApiError::internal().into_response());
    }
    let body = json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "db": "ok"
    });
    with_version(
        (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response(),
    )
}
