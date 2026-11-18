use crate::routes::{agent, health, metrics, operator, status};
use crate::state::AppState;
use crate::time::{background_interval_secs, now};
use axum::extract::DefaultBodyLimit;
use axum::http::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use lu_serve::with_request_ids;
use std::future::Future;
use std::sync::Arc;

pub struct ControllerRuntime {
    pub state: Arc<AppState>,
}

impl ControllerRuntime {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub fn router(&self) -> Router {
        build_router(Arc::clone(&self.state))
    }

    /// Runs boot recovery and periodic lease maintenance until `shutdown` resolves.
    pub async fn run_background(&self, shutdown: impl Future<Output = ()>) {
        run_background(Arc::clone(&self.state), shutdown).await;
    }
}

async fn version_mw(req: Request<axum::body::Body>, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert("x-lu-version", crate::routes::version_header());
    res
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let op_json = DefaultBodyLimit::max(crate::routes::MAX_JSON_BODY);
    let log_body = DefaultBodyLimit::max(crate::routes::MAX_LOG_CHUNK_BODY);
    let diff_body = DefaultBodyLimit::max(crate::routes::MAX_DIFF_BODY);
    let report_body = DefaultBodyLimit::max(crate::routes::MAX_REPORT_BODY);

    let operator = Router::new()
        .route(
            "/v1/jobs",
            post(operator::create_job)
                .layer(op_json)
                .get(operator::list_jobs),
        )
        .route("/v1/jobs/{id}", get(operator::get_job))
        .route(
            "/v1/jobs/{id}/cancel",
            post(operator::cancel_job).layer(op_json),
        )
        .route("/v1/jobs/{id}/logs", get(operator::get_logs))
        .route(
            "/v1/jobs/{id}/artifacts/{name}",
            get(operator::get_artifact),
        )
        .route("/v1/status", get(status::status))
        .route("/v1/nodes", get(operator::list_nodes))
        .route("/v1/nodes/{name}/stats", get(operator::get_node_stats))
        .route(
            "/v1/nodes/{name}/drain",
            post(operator::drain_node).layer(op_json),
        )
        .route(
            "/v1/nodes/{name}/undrain",
            post(operator::undrain_node).layer(op_json),
        )
        .route("/metrics", get(metrics::metrics));

    // agent routes with specific body limits
    let agent = Router::new()
        .route("/v1/agent/poll", post(agent::poll).layer(op_json))
        .route(
            "/v1/agent/jobs/{id}/start",
            post(agent::start_job).layer(op_json),
        )
        .route(
            "/v1/agent/jobs/{id}/renew",
            post(agent::renew_job).layer(op_json),
        )
        .route(
            "/v1/agent/jobs/{id}/log",
            post(agent::append_log).layer(log_body),
        )
        .route(
            "/v1/agent/jobs/{id}/artifacts/diff.patch",
            put(agent::put_diff_artifact).layer(diff_body),
        )
        .route(
            "/v1/agent/jobs/{id}/artifacts/report.json",
            put(agent::put_report_artifact).layer(report_body),
        )
        .route("/v1/agent/jobs/{id}/deps", get(agent::list_deps))
        .route(
            "/v1/agent/jobs/{id}/deps/{dep}/diff",
            get(agent::get_dep_diff),
        )
        .route(
            "/v1/agent/jobs/{id}/complete",
            post(agent::complete_job).layer(op_json),
        );

    use axum::response::IntoResponse;
    let app = Router::new()
        .route("/v1/health", get(health::health))
        .merge(operator)
        .merge(agent)
        .fallback(|| async { crate::error::ApiError::not_found("not found").into_response() })
        .with_state(state)
        .layer(middleware::from_fn(version_mw));

    with_request_ids(app)
}

pub async fn recover_on_boot(state: &Arc<AppState>) {
    let n = now();
    let res = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.recover_on_boot(n)
    })
    .await;
    match res {
        Ok(Ok(count)) => {
            state.set_recovered_on_boot(count);
            if count > 0 {
                tracing::info!(count, "recovered jobs on boot");
                state.waiters.notify_work();
            }
        }
        Ok(Err(e)) => tracing::warn!(?e, "recover_on_boot failed"),
        Err(e) => tracing::warn!(%e, "recover_on_boot task failed"),
    }
}

pub async fn maintenance_tick(state: &Arc<AppState>) {
    let n = now();
    let expired = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.expire_leases(n)
    })
    .await;
    let mut changed = false;
    match expired {
        Ok(Ok(v)) => {
            if !v.is_empty() {
                changed = true;
            }
        }
        Ok(Err(e)) => tracing::warn!(?e, "expire_leases failed"),
        Err(e) => tracing::warn!(%e, "expire_leases task failed"),
    }
    let n2 = now();
    let requeued = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let grace = state.recovery_grace_ms;
        let max = state.max_retries;
        move || store.requeue_recovering(n2, grace, max)
    })
    .await;
    match requeued {
        Ok(Ok(v)) => {
            if !v.is_empty() {
                changed = true;
                for id in &v {
                    state.waiters.notify_job(id.as_ref());
                }
            }
        }
        Ok(Err(e)) => tracing::warn!(?e, "requeue_recovering failed"),
        Err(e) => tracing::warn!(%e, "requeue_recovering task failed"),
    }
    if changed {
        state.waiters.notify_work();
    }
}

pub async fn run_background(state: Arc<AppState>, shutdown: impl Future<Output = ()>) {
    recover_on_boot(&state).await;
    let interval = std::time::Duration::from_secs(background_interval_secs(state.lease_secs));
    tokio::pin!(shutdown);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                state.waiters.begin_shutdown();
                break;
            }
            _ = ticker.tick() => {
                maintenance_tick(&state).await;
            }
        }
    }
}
