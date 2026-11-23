use crate::auth::authenticate_operator;
use crate::error::ApiError;
use crate::routes::{job_state_str, json_value_ok, store_err, with_version};
use crate::state::AppState;
use crate::time::now;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_serve::PeerAddr;
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub async fn status(
    State(state): State<Arc<AppState>>,
    peer: axum::Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let n = now();
    let counts = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || store.job_counts(n, lease_ms)
    })
    .await
    {
        Ok(Ok(counts)) => counts,
        Ok(Err(error)) => return with_version(store_err(error).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let jobs: Map<String, Value> = counts
        .jobs
        .iter()
        .map(|(job_state, count)| (job_state_str(*job_state).to_owned(), json!(count)))
        .collect();
    let oldest_queued_secs = counts
        .oldest_pending_at
        .map(|created| n.wall_unix.saturating_sub(created).max(0) as u64)
        .unwrap_or(0);
    let draft_counts = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.draft_counts()
    })
    .await
    {
        Ok(Ok(counts)) => counts,
        Ok(Err(error)) => return with_version(store_err(error).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let oldest_draft_secs = draft_counts
        .oldest_created_at
        .map(|created| n.wall_unix.saturating_sub(created).max(0) as u64)
        .unwrap_or(0);
    let node_records = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || store.nodes(n, lease_ms)
    })
    .await
    {
        Ok(Ok(nodes)) => nodes,
        Ok(Err(error)) => return with_version(store_err(error).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let providers = provider_summary(&node_records);
    let db_bytes = std::fs::metadata(&state.db_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let body = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started.elapsed().as_secs(),
        "schema_version": lu_store::SCHEMA_VERSION,
        "db_bytes": db_bytes,
        "recovered_on_boot": state.recovered_on_boot(),
        "waiting_dependents": counts.waiting_dependents,
        "jobs": jobs,
        "oldest_queued_secs": oldest_queued_secs,
        "drafts": draft_counts.total,
        "oldest_draft_secs": oldest_draft_secs,
        "nodes": counts.nodes,
        "nodes_online": counts.nodes_online,
        "providers": providers,
        "lease_secs": state.lease_secs,
        "audit_write_errors": state.audit_write_errors(),
        "scope_enabled": state.scope.enabled,
        "scope_max_depth": state.scope.max_depth,
        "scope_max_children_per_job": state.scope.max_children_per_job,
        "scope_max_descendants_per_root": state.scope.max_descendants_per_root,
    });
    with_version(json_value_ok(StatusCode::OK, body))
}

/// Per provider: how many online nodes report it ok, cooling or otherwise unusable, and the
/// soonest time a cooling one returns. Names come from node heartbeats, which the controller
/// already bounds and validates.
fn provider_summary(nodes: &[lu_store::NodeRecord]) -> Value {
    use lu_proto::ProviderState;
    let mut summary: std::collections::BTreeMap<&str, (u64, u64, u64, Option<u64>)> =
        std::collections::BTreeMap::new();
    for node in nodes.iter().filter(|node| node.online) {
        for provider in &node.heartbeat.providers {
            let entry = summary.entry(provider.name.as_str()).or_default();
            match provider.state {
                ProviderState::Ok => entry.0 += 1,
                ProviderState::Cooling => {
                    entry.1 += 1;
                    if let Some(secs) = provider.soonest_secs {
                        entry.3 = Some(entry.3.map_or(secs, |best| best.min(secs)));
                    }
                }
                _ => entry.2 += 1,
            }
        }
    }
    let object: Map<String, Value> = summary
        .into_iter()
        .map(|(name, (ok, cooling, other, soonest))| {
            (
                name.to_owned(),
                json!({
                    "ok": ok,
                    "cooling": cooling,
                    "other": other,
                    "soonest_secs": soonest,
                }),
            )
        })
        .collect();
    Value::Object(object)
}
