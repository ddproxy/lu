use crate::error::ApiError;
use crate::routes::with_version;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    // The store has no ping method, so a one-row listing stands in for it. The probe is short-TTL
    // and single-flight: an unauthenticated client cannot turn health checks into unbounded
    // blocking SQLite work.
    let db_ok = state.health.db_ok(&state.store).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use lu_config::ControllerConfig;
    use lu_proto::{ArtifactName, Completion, FailureClass, JobEdit, JobId, JobSpec, NodeName};
    use lu_store::{CordonSource, JobRecord, Now, Store, StoreError};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingStore {
        lists: AtomicU64,
    }

    impl Store for CountingStore {
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
            self.lists.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
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
            _: u32,
            _: u64,
        ) -> Result<JobRecord, StoreError> {
            unimplemented!()
        }
        fn expire_leases(&self, _: Now) -> Result<Vec<JobId>, StoreError> {
            unimplemented!()
        }
        fn requeue_recovering(
            &self,
            _: Now,
            _: u64,
            _: u32,
            _: u32,
        ) -> Result<Vec<JobId>, StoreError> {
            unimplemented!()
        }
        fn recover_on_boot(&self, _: Now) -> Result<u64, StoreError> {
            unimplemented!()
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
    async fn concurrent_health_checks_share_one_db_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = ControllerConfig {
            listen: "127.0.0.1:0".to_owned(),
            db: tmp.path().join("lu.db").display().to_string(),
            artifacts_dir: tmp.path().join("artifacts").display().to_string(),
            lease_secs: 6,
            max_retries: 1,
            node_attempts: 2,
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
        let store = Arc::new(CountingStore {
            lists: AtomicU64::new(0),
        });
        let state = Arc::new(AppState::from_config(&cfg, store.clone()).unwrap());
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move { health(State(state)).await }));
        }
        for task in tasks {
            let response = task.await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(
            store.lists.load(Ordering::SeqCst),
            1,
            "health checks must share one short-TTL DB probe"
        );
    }
}
