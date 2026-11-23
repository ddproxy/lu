//! Operator-facing node contribution statistics. All values come from the same store report the
//! CLI and `/metrics` use.

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_proto::{parse_since, NodeName, StatsGroup, StatsQuery};
use lu_serve::PeerAddr;
use std::sync::Arc;

use crate::error::ApiError;
use crate::read_api::authenticate_read;
use crate::routes::{json_ok, with_version};
use crate::state::AppState;
use crate::time::now;

pub async fn stats(
    State(state): State<Arc<AppState>>,
    peer: Extension<PeerAddr>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if let Err(error) = authenticate_read(&headers, peer.0, &state) {
        return with_version(error.into_response());
    }
    let query_parts = crate::routes::parse_query(&uri);
    let by = match query_parts.get("by").map(String::as_str) {
        Some(value) => match value.parse::<StatsGroup>() {
            Ok(group) => group,
            Err(error) => {
                return with_version(ApiError::bad_request(error.to_string()).into_response())
            }
        },
        None => StatsGroup::default(),
    };
    let since = match query_parts.get("since") {
        Some(value) => match parse_since(value, now().wall_unix) {
            Ok(since) => Some(since),
            Err(error) => return with_version(ApiError::bad_request(error).into_response()),
        },
        None => None,
    };
    let node = match query_parts.get("node") {
        Some(value) => match value.parse::<NodeName>() {
            Ok(node) => Some(node),
            Err(error) => {
                return with_version(ApiError::bad_request(error.to_string()).into_response())
            }
        },
        None => None,
    };

    let mut nodes: Vec<NodeName> = state.auth.agents.values().cloned().collect();
    nodes.sort();
    let query = StatsQuery {
        by,
        since,
        node,
        nodes,
    };
    let report = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.stats(&query)
    })
    .await
    {
        Ok(Ok(report)) => report,
        _ => return with_version(ApiError::internal().into_response()),
    };
    with_version(json_ok(StatusCode::OK, &report))
}
