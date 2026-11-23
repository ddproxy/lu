use crate::routes::{agent, health, metrics, operator, schedule, scope, stats, status};
use crate::state::AppState;
use crate::time::{background_interval_secs, now};
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use lu_serve::with_request_ids;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

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

    /// Runs periodic lease maintenance until `shutdown` resolves.
    ///
    /// Boot recovery must be run separately (and awaited) before the service starts:
    /// recovering with stale leases can otherwise reassign work twice.
    pub async fn run_background(&self, shutdown: impl Future<Output = ()>) {
        run_background(Arc::clone(&self.state), shutdown).await;
    }
}

async fn version_mw(req: Request<axum::body::Body>, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert("x-lu-version", crate::routes::version_header());
    // Applies to every route, including the fallback and unauthenticated health check. Raw
    // artifact and log responses set Content-Disposition themselves.
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let op_json = DefaultBodyLimit::max(crate::routes::MAX_JSON_BODY);
    let log_body = DefaultBodyLimit::max(crate::routes::MAX_LOG_CHUNK_BODY);
    let diff_body = DefaultBodyLimit::max(crate::routes::MAX_DIFF_BODY);
    let report_body = DefaultBodyLimit::max(crate::routes::MAX_REPORT_BODY);
    // Bound body buffering by the configured intake cap, not a flat 16 MiB: an unsigned caller
    // cannot make a small-cap source buffer a much larger request before its cap is checked.
    let intake_body = DefaultBodyLimit::max(
        state
            .intake
            .as_ref()
            .map_or(0, |intake| intake.max_body_bytes_limit()),
    );

    let operator = Router::new()
        .route(
            "/v1/jobs",
            post(operator::create_job)
                .layer(op_json)
                .get(operator::list_jobs),
        )
        .route("/v1/jobs/{id}", get(operator::get_job))
        .route(
            "/v1/jobs/{id}/publish",
            post(operator::publish_job).layer(op_json),
        )
        .route(
            "/v1/jobs/{id}/retry",
            post(operator::retry_job).layer(op_json),
        )
        .route(
            "/v1/groups/{name}/limit",
            put(operator::set_group_limit).layer(op_json),
        )
        .route(
            "/v1/jobs/{id}/cancel",
            post(operator::cancel_job).layer(op_json),
        )
        .route(
            "/v1/jobs/{id}/edit",
            post(operator::edit_job).layer(op_json),
        )
        .route("/v1/jobs/{id}/logs", get(operator::get_logs))
        .route(
            "/v1/jobs/{id}/artifacts/{name}",
            get(operator::get_artifact),
        )
        .route("/v1/drafts", get(operator::list_drafts))
        .route("/v1/drafts/{id}", put(operator::edit_job).layer(op_json))
        .route(
            "/v1/drafts/{id}/approve",
            post(operator::approve_draft).layer(op_json),
        )
        .route(
            "/v1/drafts/{id}/reject",
            post(operator::reject_draft).layer(op_json),
        )
        .route("/v1/status", get(status::status))
        .route("/v1/stats", get(stats::stats))
        .route("/v1/repos", get(operator::list_repos))
        .route(
            "/v1/repos/{name}/sync",
            post(operator::sync_repo).layer(op_json),
        )
        .route("/v1/nodes", get(operator::list_nodes))
        .route("/v1/schedule", get(schedule::show))
        .route("/v1/schedule/calendars", get(schedule::list_calendars))
        .route("/v1/schedule/calendars/{name}", get(schedule::get_calendar))
        .route(
            "/v1/schedule/explain",
            post(schedule::explain).layer(op_json),
        )
        .route("/v1/schedule/reload", post(schedule::reload))
        .route("/v1/nodes/{name}/stats", get(operator::get_node_stats))
        .route(
            "/v1/nodes/{name}/workspaces/clean",
            post(operator::clean_workspaces),
        )
        .route(
            "/v1/nodes/{name}/drain",
            post(operator::drain_node).layer(op_json),
        )
        .route(
            "/v1/nodes/{name}/undrain",
            post(operator::undrain_node).layer(op_json),
        )
        .route(
            "/v1/nodes/{name}/uncordon",
            post(operator::uncordon_node).layer(op_json),
        )
        .route("/metrics", get(metrics::metrics));

    // agent routes with specific body limits
    let agent = Router::new()
        .route("/v1/agent/poll", post(agent::poll).layer(op_json))
        .route("/v1/agent/self", get(agent::self_view))
        .route("/v1/agent/drain", post(agent::drain).layer(op_json))
        .route("/v1/agent/undrain", post(agent::undrain).layer(op_json))
        .route("/v1/agent/jobs/{id}/base", get(agent::job_base))
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
        .route("/v1/scope/{route}", post(scope::handle))
        .route(
            "/v1/agent/jobs/{id}/workspace/release",
            get(agent::workspace_release),
        )
        .route("/v1/agent/jobs/{id}/deps", get(agent::list_deps))
        .route(
            "/v1/agent/jobs/{id}/staged-commit",
            post(agent::set_staged_commit).layer(op_json),
        )
        .route(
            "/v1/agent/jobs/{id}/merge-base",
            post(agent::merge_base).layer(op_json),
        )
        .route(
            "/v1/agent/jobs/{id}/deps/{dep}/diff",
            get(agent::get_dep_diff),
        )
        .route(
            "/v1/agent/jobs/{id}/deps/{dep}/artifacts/{name}",
            get(agent::get_dep_artifact),
        )
        .route(
            "/v1/agent/jobs/{id}/complete",
            post(agent::complete_job).layer(op_json),
        );

    use axum::response::IntoResponse;
    let mut app = Router::new()
        .route("/v1/health", get(health::health))
        .merge(operator)
        .merge(agent)
        .fallback(|| async { crate::error::ApiError::not_found("not found").into_response() });

    // Intake routes exist only when `[intake]` is configured. They authenticate with a per-source
    // HMAC key, never with an operator token.
    if state.intake.is_some() {
        app = app.merge(Router::new().route(
            "/v1/intake/{source}",
            post(crate::routes::intake::intake).layer(intake_body),
        ));
    }
    let app = app
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::role::role_guard,
        ))
        .with_state(state)
        .layer(middleware::from_fn(version_mw));

    with_request_ids(app)
}

/// Moves every leased or running job to `recovering` before any requests are served.
///
/// Failure is fatal to startup: serving with stale leases would let a new process
/// hand the same job to a second agent after the old process's monotonic clock
/// has reset.
pub async fn recover_on_boot(state: &Arc<AppState>) -> Result<u64, String> {
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
            Ok(count)
        }
        Ok(Err(e)) => Err(format!("recover_on_boot failed: {e:?}")),
        Err(e) => Err(format!("recover_on_boot task failed: {e}")),
    }
}

pub async fn maintenance_tick(state: &Arc<AppState>) {
    let n = now();
    if !state.note_wall_tick(n.wall_unix) {
        tracing::warn!(
            wall_unix = n.wall_unix,
            "wall clock moved backwards; leases stay on monotonic time"
        );
        state.set_schedule_wake(None);
    }
    if let Some(schedule) = state.schedule() {
        let store = Arc::clone(&state.store);
        match tokio::task::spawn_blocking(move || store.expire_deadlines(n, &schedule)).await {
            Ok(Ok(expired)) => {
                for (id, action) in expired {
                    state.note_deadline_expired();
                    state.waiters.notify_job(id.as_ref());
                    state.record_audit(
                        lu_audit::Event::DeadlineMissed,
                        Some(id.as_str()),
                        None,
                        action.as_str(),
                        lu_audit::Data::new().text(lu_audit::Field::Outcome, action.as_str()),
                    );
                }
            }
            Ok(Err(e)) => tracing::warn!(?e, "expire_deadlines failed"),
            Err(e) => tracing::warn!(%e, "expire_deadlines task failed"),
        }
    }
    let cutoff = n.wall_unix.saturating_sub(state.drafts.ttl_secs as i64);
    let expired_drafts = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.expire_drafts(cutoff, n)
    })
    .await;
    match expired_drafts {
        Ok(Ok(count)) if count > 0 => {
            state.record_audit(
                lu_audit::Event::DraftExpired,
                None,
                None,
                "drafts",
                lu_audit::Data::new().unsigned(lu_audit::Field::Count, count),
            );
        }
        Ok(Err(e)) => tracing::warn!(?e, "expire_drafts failed"),
        Err(e) => tracing::warn!(%e, "expire_drafts task failed"),
        _ => {}
    }
    let mut changed = false;
    let expired_dependents = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        let ttl_secs = state.blocked_dependent_ttl_secs;
        move || store.expire_blocked_dependents(n, ttl_secs)
    })
    .await;
    match expired_dependents {
        Ok(Ok(ids)) => {
            if !ids.is_empty() {
                changed = true;
                for id in &ids {
                    state.waiters.notify_job(id.as_ref());
                }
            }
        }
        Ok(Err(e)) => tracing::warn!(?e, "expire_blocked_dependents failed"),
        Err(e) => tracing::warn!(%e, "expire_blocked_dependents task failed"),
    }
    let expired = tokio::task::spawn_blocking({
        let store = Arc::clone(&state.store);
        move || store.expire_leases(n)
    })
    .await;
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
    if let Some(schedule) = state.schedule() {
        let store = Arc::clone(&state.store);
        let queued = tokio::task::spawn_blocking(move || {
            store.list_jobs(Some(lu_proto::JobState::Queued), 10_000, None)
        })
        .await;
        let mut next = schedule.next_change(n.wall_unix);
        if let Ok(Ok(jobs)) = queued {
            for job in jobs {
                let Some(js) = &job.spec.schedule else {
                    continue;
                };
                for at in [js.not_before, js.deadline].into_iter().flatten() {
                    if at > n.wall_unix {
                        next = Some(next.map_or(at, |current| current.min(at)));
                    }
                }
            }
        }
        state.set_schedule_wake(next);
    } else {
        state.set_schedule_wake(None);
    }
}

pub async fn run_background(state: Arc<AppState>, shutdown: impl Future<Output = ()>) {
    let interval = std::time::Duration::from_secs(background_interval_secs(state.lease_secs));
    tokio::pin!(shutdown);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // Re-arm from the boundary recorded by the last tick. A clock fault clears that value and
        // the next maintenance tick recomputes it from the corrected wall time.
        let wake = state
            .schedule_wake()
            .map_or(Duration::from_secs(3600), |at| {
                Duration::from_secs(at.saturating_sub(now().wall_unix).clamp(1, 3600) as u64)
            });
        tokio::select! {
            _ = &mut shutdown => {
                state.waiters.begin_shutdown();
                break;
            }
            _ = ticker.tick() => {
                maintenance_tick(&state).await;
            }
            _ = tokio::time::sleep(wake) => {
                maintenance_tick(&state).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lu_config::ControllerConfig;
    use lu_proto::{ArtifactName, Completion, FailureClass, JobEdit, JobId, JobSpec, NodeName};
    use lu_store::{CordonSource, JobRecord, Now, Store, StoreError};
    use std::collections::HashMap;

    struct FailingRecoveryStore;

    impl Store for FailingRecoveryStore {
        fn create_job(&self, _: &JobSpec, _: Now) -> Result<(JobRecord, bool), StoreError> {
            unimplemented!()
        }
        fn get_job(&self, _: &JobId) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn job_by_key(&self, _: &str) -> Option<JobRecord> {
            None
        }
        fn list_jobs(
            &self,
            _: Option<lu_proto::JobState>,
            _: usize,
            _: Option<&JobId>,
        ) -> Result<Vec<JobRecord>, StoreError> {
            unimplemented!()
        }
        fn cancel_job(&self, _: &JobId, _: Now) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn edit_job(&self, _: &JobId, _: &JobEdit, _: Now) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn heartbeat(&self, _: &lu_proto::Heartbeat, _: Now) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn next_assignment(
            &self,
            _: &NodeName,
            _: Now,
            _: u64,
        ) -> Result<Option<lu_store::Assignment>, StoreError> {
            unimplemented!()
        }
        fn pending_cancels(&self, _: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
            unimplemented!()
        }
        fn held_jobs(&self, _: &NodeName) -> Result<Vec<(JobId, u64)>, StoreError> {
            unimplemented!()
        }
        fn start_job(&self, _: &JobId, _: u64, _: Now) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn renew_lease(&self, _: &JobId, _: u64, _: Now, _: u64) -> Result<bool, StoreError> {
            unimplemented!()
        }
        fn reserve_log(&self, _: &JobId, _: u64, _: u64, _: u64) -> Result<u64, StoreError> {
            unimplemented!()
        }
        fn unreserve_log(&self, _: &JobId, _: u64, _: u64, _: u64) -> Result<u64, StoreError> {
            unimplemented!()
        }
        fn record_artifact(
            &self,
            _: &JobId,
            _: u64,
            _: ArtifactName,
            _: u64,
        ) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn record_scrub(&self, _: &JobId, _: u64, _: u64, _: bool) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn record_patch_secret_hits(
            &self,
            _: &JobId,
            _: u64,
            _: &lu_proto::view::PatchSecretHitsView,
        ) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn record_publish(
            &self,
            _: &JobId,
            _: &lu_proto::PublishRecord,
            _: Now,
        ) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn complete(
            &self,
            _: &Completion,
            _: Option<(FailureClass, Option<String>)>,
            _: Now,
            _: u32,
            _: u64,
        ) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn expire_leases(&self, _: Now) -> Result<Vec<JobId>, StoreError> {
            unimplemented!()
        }
        fn requeue_recovering(&self, _: Now, _: u64, _: u32) -> Result<Vec<JobId>, StoreError> {
            unimplemented!()
        }
        fn recover_on_boot(&self, _: Now) -> Result<u64, StoreError> {
            Err(StoreError::Internal("recovery unavailable".to_owned()))
        }
        fn set_draining(&self, _: &NodeName, _: bool) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn set_agent_drain(&self, _: &NodeName) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn clear_agent_drain(&self, _: &NodeName) -> Result<bool, StoreError> {
            unimplemented!()
        }
        fn nodes(&self, _: Now, _: u64) -> Result<Vec<lu_store::NodeRecord>, StoreError> {
            unimplemented!()
        }
        fn cordons(&self, _: Option<&NodeName>) -> Result<Vec<lu_store::Cordon>, StoreError> {
            unimplemented!()
        }
        fn set_cordon(
            &self,
            _: &NodeName,
            _: &str,
            _: CordonSource,
            _: &str,
            _: i64,
            _: i64,
        ) -> Result<(), StoreError> {
            unimplemented!()
        }
        fn uncordon(&self, _: &NodeName, _: Option<&str>) -> Result<Vec<String>, StoreError> {
            unimplemented!()
        }
        fn job_counts(&self, _: Now, _: u64) -> Result<lu_store::JobCounts, StoreError> {
            unimplemented!()
        }
        fn set_group_limits(&self, _: HashMap<String, usize>) -> Result<(), StoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn boot_recovery_failure_stops_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = ControllerConfig {
            listen: "127.0.0.1:0".to_owned(),
            db: tmp.path().join("lu.db").display().to_string(),
            artifacts_dir: tmp.path().join("artifacts").display().to_string(),
            lease_secs: 6,
            max_retries: 1,
            provider_cooldown_secs: 60,
            recovery_grace_secs: 1,
            max_connections: 8,
            max_connections_per_addr: 4,
            local_repo_roots: Vec::new(),
            agents: Vec::new(),
            operators: Vec::new(),
            repos: Vec::new(),
            groups: Vec::new(),
            blocked_dependent_ttl: "24h".to_owned(),
            read_per_minute: lu_config::DEFAULT_READ_PER_MINUTE,
        };
        let state = Arc::new(AppState::from_config(&cfg, Arc::new(FailingRecoveryStore)).unwrap());
        let error = recover_on_boot(&state).await.unwrap_err();
        assert!(error.contains("recovery unavailable"), "{error}");
        assert_eq!(state.recovered_on_boot(), 0);
    }
}
