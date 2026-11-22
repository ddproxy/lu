use crate::auth::authenticate_operator;
use crate::error::ApiError;
use crate::routes::operator::cordon_until_wall;
use crate::routes::with_version;
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_proto::StuckPolicy;
use lu_serve::PeerAddr;
use lu_store::CordonSource;
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

    let group_counts = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.group_counts()
    })
    .await
    {
        Ok(Ok(c)) => c,
        _ => return with_version(ApiError::internal().into_response()),
    };

    let mut out = String::new();
    let group_limits = state.group_limits();

    let version = escape_label(env!("CARGO_PKG_VERSION"));
    out.push_str("# HELP lu_build_info Controller build version\n");
    out.push_str("# TYPE lu_build_info gauge\n");
    out.push_str(&gauge("lu_build_info", &[("version", &version)], 1));
    out.push('\n');

    out.push_str("# HELP lu_uptime_seconds Seconds since the controller started\n");
    out.push_str("# TYPE lu_uptime_seconds gauge\n");
    out.push_str(&gauge(
        "lu_uptime_seconds",
        &[],
        state.started.elapsed().as_secs(),
    ));
    out.push('\n');

    out.push_str("# HELP lu_db_bytes Size of the SQLite database file\n");
    out.push_str("# TYPE lu_db_bytes gauge\n");
    let db_bytes = std::fs::metadata(&state.db_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    out.push_str(&gauge("lu_db_bytes", &[], db_bytes));
    out.push('\n');

    out.push_str("# HELP lu_schema_version Database schema version\n");
    out.push_str("# TYPE lu_schema_version gauge\n");
    out.push_str(&gauge(
        "lu_schema_version",
        &[],
        lu_store::SCHEMA_VERSION as u64,
    ));
    out.push('\n');

    out.push_str("# HELP lu_recovered_on_boot Jobs moved to recovery at startup\n");
    out.push_str("# TYPE lu_recovered_on_boot gauge\n");
    out.push_str(&gauge(
        "lu_recovered_on_boot",
        &[],
        state.recovered_on_boot(),
    ));
    out.push('\n');

    out.push_str("# HELP lu_queue_oldest_seconds Age of the oldest queued or blocked job\n");
    out.push_str("# TYPE lu_queue_oldest_seconds gauge\n");
    let oldest = counts
        .oldest_pending_at
        .map(|created| n.wall_unix.saturating_sub(created).max(0) as u64)
        .unwrap_or(0);
    out.push_str(&gauge("lu_queue_oldest_seconds", &[], oldest));
    out.push('\n');

    out.push_str(
        "# HELP lu_blocked_dependents_total Blocked jobs waiting for a retry or TTL expiry\n",
    );
    out.push_str("# TYPE lu_blocked_dependents_total gauge\n");
    out.push_str(&gauge(
        "lu_blocked_dependents_total",
        &[],
        counts.waiting_dependents,
    ));
    out.push('\n');

    // HELP and TYPE comments for gauges
    out.push_str("# HELP lu_jobs Number of jobs per state\n");
    out.push_str("# TYPE lu_jobs gauge\n");
    for (state, count) in &counts.jobs {
        out.push_str(&gauge("lu_jobs", &[("state", state.as_str())], *count));
        out.push('\n');
    }

    out.push_str("# HELP lu_drafts Draft and expired jobs by state\n");
    out.push_str("# TYPE lu_drafts gauge\n");
    for (state, count) in &counts.jobs {
        if matches!(
            state,
            lu_proto::JobState::Draft | lu_proto::JobState::Expired
        ) {
            out.push_str(&gauge("lu_drafts", &[("state", state.as_str())], *count));
            out.push('\n');
        }
    }

    if !group_limits.is_empty() {
        out.push_str("# HELP lu_group_running Jobs active in a capped group\n");
        out.push_str("# TYPE lu_group_running gauge\n");
        out.push_str("# HELP lu_group_limit Active concurrency cap for a group\n");
        out.push_str("# TYPE lu_group_limit gauge\n");
        let mut names: Vec<_> = group_limits.keys().collect();
        names.sort();
        for name in names {
            let label = escape_label(name);
            let running = group_counts.get(name).copied().unwrap_or(0) as u64;
            let limit = group_limits.get(name).copied().unwrap_or(0) as u64;
            out.push_str(&gauge("lu_group_running", &[("group", &label)], running));
            out.push('\n');
            out.push_str(&gauge("lu_group_limit", &[("group", &label)], limit));
            out.push('\n');
        }
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

    // Workspace lifecycle counters from the latest agent heartbeats.
    let mut node_stats: Vec<_> = state
        .stats
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(node, stats)| (node.to_string(), stats.clone()))
        .collect();
    node_stats.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, help, kind, value) in [
        (
            "lu_node_workspace_bytes",
            "Bytes in a node's job workspaces",
            "gauge",
            0,
        ),
        (
            "lu_node_workspace_count",
            "Number of job workspaces on a node",
            "gauge",
            1,
        ),
        (
            "lu_node_workspace_reclaimed_bytes_total",
            "Bytes reclaimed by a node's workspace cleanup",
            "counter",
            2,
        ),
        (
            "lu_node_workspace_evictions_total",
            "Workspaces removed by a node's eviction sweeps",
            "counter",
            3,
        ),
        (
            "lu_node_workspace_delete_errors_total",
            "Failed workspace deletions on a node",
            "counter",
            4,
        ),
    ] {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        for (node, stats) in &node_stats {
            let label = escape_label(node);
            let number = match value {
                0 => stats.workspace_bytes,
                1 => stats.workspace_count,
                2 => stats.workspace_reclaimed_bytes_total,
                3 => stats.workspace_evictions_total,
                _ => stats.workspace_delete_errors_total,
            };
            let line = if kind == "counter" {
                counter(name, &[("node", &label)], number)
            } else {
                gauge(name, &[("node", &label)], number)
            };
            out.push_str(&line);
            out.push('\n');
        }
    }

    out.push_str("# HELP lu_controller_up Whether the controller is up (1) or down (0)\n");
    out.push_str("# TYPE lu_controller_up gauge\n");
    out.push_str("lu_controller_up 1\n");

    out.push_str(
        "# HELP lu_repo_last_sync_seconds Seconds since each repository was last mirrored\n",
    );
    out.push_str("# TYPE lu_repo_last_sync_seconds gauge\n");
    for repo in state.mirror.infos() {
        let name = escape_label(&repo.name);
        let age = repo
            .synced_at
            .map(|at| n.wall_unix.saturating_sub(at).max(0) as u64)
            .unwrap_or(0);
        out.push_str(&gauge("lu_repo_last_sync_seconds", &[("repo", &name)], age));
        out.push('\n');
    }

    out.push_str("# HELP lu_intake_total Intake requests by source and result\n");
    out.push_str("# TYPE lu_intake_total counter\n");
    for (source, result, value) in state.intake_totals() {
        let source = escape_label(&source);
        let result = escape_label(&result);
        out.push_str(&counter(
            "lu_intake_total",
            &[("source", &source), ("result", &result)],
            value,
        ));
        out.push('\n');
    }

    // Counters
    out.push_str("# HELP lu_jobs_completed_total Total number of jobs completed per outcome\n");
    out.push_str("# TYPE lu_jobs_completed_total counter\n");
    out.push_str(&counter(
        "lu_jobs_completed_total",
        &[("outcome", lu_proto::JobState::Done.as_str())],
        counts.completed_done,
    ));
    out.push('\n');
    out.push_str(&counter(
        "lu_jobs_completed_total",
        &[("outcome", lu_proto::JobState::Failed.as_str())],
        counts.completed_failed,
    ));
    out.push('\n');
    out.push_str(&counter(
        "lu_jobs_completed_total",
        &[("outcome", lu_proto::JobState::Cancelled.as_str())],
        counts.completed_cancelled,
    ));
    out.push('\n');

    out.push_str(
        "# HELP lu_jobs_no_changes_total Done jobs with an empty diff and no allow_empty opt-out\n",
    );
    out.push_str("# TYPE lu_jobs_no_changes_total counter\n");
    out.push_str(&counter(
        "lu_jobs_no_changes_total",
        &[],
        state.no_changes_total(),
    ));
    out.push('\n');

    out.push_str(
        "# HELP lu_jobs_expect_failed_total Done jobs with unmet expectations in suspect mode\n",
    );
    out.push_str("# TYPE lu_jobs_expect_failed_total counter\n");
    out.push_str(&counter(
        "lu_jobs_expect_failed_total",
        &[],
        state.expect_failed_total(),
    ));
    out.push('\n');

    out.push_str("# HELP lu_jobs_stuck_stopped_total Jobs stopped by the agent's stuck policy\n");
    out.push_str("# TYPE lu_jobs_stuck_stopped_total counter\n");
    for mode in [StuckPolicy::Cancel, StuckPolicy::Retry] {
        out.push_str(&counter(
            "lu_jobs_stuck_stopped_total",
            &[("mode", mode.as_str())],
            state.stuck_stopped_total(mode),
        ));
        out.push('\n');
    }

    out.push_str("# HELP lu_scope_requests_total Job-scoped actions by route and result\n");
    out.push_str("# TYPE lu_scope_requests_total counter\n");
    let mut scope_rows = state.scope_request_counts();
    scope_rows.sort_by(|a, b| a.0.cmp(&b.0));
    for ((route, result), value) in &scope_rows {
        let route = escape_label(route);
        let result = escape_label(result);
        out.push_str(&counter(
            "lu_scope_requests_total",
            &[("route", &route), ("result", &result)],
            *value,
        ));
        out.push('\n');
    }

    out.push_str(
        "# HELP lu_scope_limit_exceeded_total Job-scoped requests refused by a named limit\n",
    );
    out.push_str("# TYPE lu_scope_limit_exceeded_total counter\n");
    for (limit, value) in state.scope_limit_counts() {
        let label = escape_label(&limit);
        out.push_str(&counter(
            "lu_scope_limit_exceeded_total",
            &[("limit", &label)],
            value,
        ));
        out.push('\n');
    }

    // lu_node_providers{node,provider,state} - need to query nodes with providers
    // Get node records
    let (nodes, cordons) = match tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let lease_ms = state.lease_ms;
        move || {
            let nodes = store.nodes(n, lease_ms)?;
            let cordons = store.cordons(None)?;
            Ok::<_, lu_store::StoreError>((nodes, cordons))
        }
    })
    .await
    {
        Ok(Ok(pair)) => pair,
        _ => return with_version(ApiError::internal().into_response()),
    };

    out.push_str("# HELP lu_node_last_seen_seconds Seconds since each node last reported\n");
    out.push_str("# TYPE lu_node_last_seen_seconds gauge\n");
    for node_rec in &nodes {
        let node_name = escape_label(node_rec.name.as_str());
        let age = n.wall_unix.saturating_sub(node_rec.last_seen).max(0) as u64;
        out.push_str(&gauge(
            "lu_node_last_seen_seconds",
            &[("node", &node_name)],
            age,
        ));
        out.push('\n');
    }

    out.push_str("# HELP lu_node_labels_info Node labels advertised in the latest heartbeat\n");
    out.push_str("# TYPE lu_node_labels_info gauge\n");
    for node_rec in &nodes {
        let node_name = escape_label(node_rec.name.as_str());
        for label in &node_rec.heartbeat.labels {
            let label = escape_label(label);
            out.push_str(&gauge(
                "lu_node_labels_info",
                &[("node", &node_name), ("label", &label)],
                1,
            ));
            out.push('\n');
        }
    }

    out.push_str("# HELP lu_node_disk_free_percent Free space on each node's work filesystem\n");
    out.push_str("# TYPE lu_node_disk_free_percent gauge\n");
    for node_rec in &nodes {
        let Some(host) = node_rec.heartbeat.host.as_ref() else {
            continue;
        };
        let (Some(free), Some(total)) = (host.disk_free_bytes, host.disk_total_bytes) else {
            continue;
        };
        let Some(percent) = free.saturating_mul(100).checked_div(total) else {
            continue;
        };
        let node_name = escape_label(node_rec.name.as_str());
        out.push_str(&gauge(
            "lu_node_disk_free_percent",
            &[("node", &node_name)],
            percent,
        ));
        out.push('\n');
    }

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
            out.push_str(&gauge(
                "lu_node_providers",
                &[
                    ("node", &node_name),
                    ("provider", &prov_name),
                    ("state", prov.state.as_str()),
                ],
                1,
            ));
            out.push('\n');
        }
    }

    out.push_str(
        "# HELP lu_node_cordon_seconds_remaining Seconds left on a provider cordon; 0 when it lifts on healthy heartbeats\n",
    );
    out.push_str("# TYPE lu_node_cordon_seconds_remaining gauge\n");
    for cordon in &cordons {
        let node_name = escape_label(cordon.node.as_str());
        let provider = escape_label(&cordon.provider);
        let source = cordon.source.as_str();
        let remaining = match (cordon.source, cordon.until_mono_ms) {
            (CordonSource::Failure, Some(until)) => {
                (cordon_until_wall(until, n).saturating_sub(n.wall_unix)).max(0) as u64
            }
            _ => 0,
        };
        out.push_str(&gauge(
            "lu_node_cordon_seconds_remaining",
            &[
                ("node", &node_name),
                ("provider", &provider),
                ("source", source),
            ],
            remaining,
        ));
        out.push('\n');
    }

    out.push_str("# HELP lu_publish_total Publish attempts by target and result\n");
    out.push_str("# TYPE lu_publish_total counter\n");
    for (target, result, value) in state.publish_totals() {
        let target = escape_label(&target);
        let result = escape_label(&result);
        out.push_str(&counter(
            "lu_publish_total",
            &[("target", &target), ("result", &result)],
            value,
        ));
        out.push('\n');
    }

    out.push_str(
        "# HELP lu_audit_write_errors_total Audit record enqueue or write failures since this controller started\n",
    );
    out.push_str("# TYPE lu_audit_write_errors_total counter\n");
    out.push_str(&counter(
        "lu_audit_write_errors_total",
        &[],
        state.audit_write_errors(),
    ));
    out.push('\n');

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
