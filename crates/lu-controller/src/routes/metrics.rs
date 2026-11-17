use crate::auth::authenticate_operator;
use crate::error::ApiError;
use crate::routes::with_version;
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_proto::ProviderState;
use lu_serve::PeerAddr;
use std::sync::Arc;

/// Escape label value for Prometheus text format.
/// Escape backslash, double-quote, and newline.
fn escape_label(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 4);
    for c in v.chars() {
        match c {
            '\\' => out.push_str(r#"\\"#),
            '"' => out.push_str(r#"\""#),
            '\n' => out.push_str(r#"\n"#),
            _ => out.push(c),
        }
    }
    out
}

/// Format a gauge line: `name{labels} value`
fn gauge(name: &str, labels: &[(&str, &str)], value: u64) -> String {
    if labels.is_empty() {
        format!("{name} {value}")
    } else {
        let label_str = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!("{name}{{{label_str}}} {value}")
    }
}

/// Format a counter line: `name{labels} value`
fn counter(name: &str, labels: &[(&str, &str)], value: u64) -> String {
    gauge(name, labels, value)
}

pub async fn metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Extension<PeerAddr>,
) -> Response {
    let _ = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(o) => o,
        Err(e) => return with_version(e.into_response()),
    };

    let n = now();
    let counts = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || store.job_counts(n, lease_ms)
    })
    .await
    {
        Ok(Ok(c)) => c,
        _ => return with_version(ApiError::internal().into_response()),
    };

    let mut out = String::new();

    // HELP and TYPE comments for gauges
    out.push_str("# HELP lu_jobs Number of jobs per state\n");
    out.push_str("# TYPE lu_jobs gauge\n");
    for (state, count) in &counts.jobs {
        let state_str = match state {
            lu_proto::JobState::Blocked => "blocked",
            lu_proto::JobState::Queued => "queued",
            lu_proto::JobState::Leased => "leased",
            lu_proto::JobState::Running => "running",
            lu_proto::JobState::Recovering => "recovering",
            lu_proto::JobState::NeedsInput => "needs_input",
            lu_proto::JobState::Done => "done",
            lu_proto::JobState::Failed => "failed",
            lu_proto::JobState::Cancelled => "cancelled",
        };
        out.push_str(&gauge("lu_jobs", &[("state", state_str)], *count));
        out.push('\n');
    }

    out.push_str("# HELP lu_nodes Total number of nodes known\n");
    out.push_str("# TYPE lu_nodes gauge\n");
    out.push_str(&gauge("lu_nodes", &[], counts.nodes));
    out.push('\n');

    out.push_str("# HELP lu_nodes_online Number of nodes seen within two lease periods\n");
    out.push_str("# TYPE lu_nodes_online gauge\n");
    out.push_str(&gauge("lu_nodes_online", &[], counts.nodes_online));
    out.push('\n');

    out.push_str("# HELP lu_nodes_drained Number of nodes with draining=true\n");
    out.push_str("# TYPE lu_nodes_drained gauge\n");
    out.push_str(&gauge("lu_nodes_drained", &[], counts.nodes_drained));
    out.push('\n');

    out.push_str("# HELP lu_controller_up Whether the controller is up (1) or down (0)\n");
    out.push_str("# TYPE lu_controller_up gauge\n");
    out.push_str("lu_controller_up 1\n");

    // Counters
    out.push_str("# HELP lu_jobs_completed_total Total number of jobs completed per outcome\n");
    out.push_str("# TYPE lu_jobs_completed_total counter\n");
    out.push_str(&counter(
        "lu_jobs_completed_total",
        &[("outcome", "done")],
        counts.completed_done,
    ));
    out.push('\n');
    out.push_str(&counter(
        "lu_jobs_completed_total",
        &[("outcome", "failed")],
        counts.completed_failed,
    ));
    out.push('\n');
    out.push_str(&counter(
        "lu_jobs_completed_total",
        &[("outcome", "cancelled")],
        counts.completed_cancelled,
    ));
    out.push('\n');

    // lu_node_providers{node,provider,state} - need to query nodes with providers
    // Get node records
    let nodes = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || store.nodes(n, lease_ms)
    })
    .await
    {
        Ok(Ok(n)) => n,
        _ => return with_version(ApiError::internal().into_response()),
    };

    out.push_str("# HELP lu_node_providers Provider state per node (1 if reported, 0 otherwise)\n");
    out.push_str("# TYPE lu_node_providers gauge\n");
    for node_rec in &nodes {
        let node_name = escape_label(node_rec.name.as_str());
        if node_rec.heartbeat.providers.is_empty() {
            // no providers reported, still emit something? spec says "as 1 for each reported provider state"
            // so we skip if none reported
            continue;
        }
        for prov in &node_rec.heartbeat.providers {
            let prov_name = escape_label(&prov.name);
            let state_str = match prov.state {
                ProviderState::Ok => "ok",
                ProviderState::Cooling => "cooling",
                ProviderState::Disabled => "disabled",
                ProviderState::Unknown => "unknown",
            };
            out.push_str(&gauge(
                "lu_node_providers",
                &[
                    ("node", &node_name),
                    ("provider", &prov_name),
                    ("state", state_str),
                ],
                1,
            ));
            out.push('\n');
        }
    }

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_label_works() {
        assert_eq!(escape_label("simple"), "simple");
        assert_eq!(escape_label(r#"has"quote"#), r#"has\"quote"#);
        assert_eq!(escape_label("has\\backslash"), "has\\\\backslash");
        assert_eq!(escape_label("has\nnewline"), "has\\nnewline");
        assert_eq!(escape_label(r#"mixed\"stuff\n"#), r#"mixed\\\"stuff\\n"#);
    }
}
