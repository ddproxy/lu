use crate::auth::AuthConfig;
use crate::limits::{TokenLimiter, PACK_CONCURRENCY, SYNC_CONCURRENCY};
use crate::mirror::Mirror;
use crate::repo::{LocalPathProvider, RepoProvider};
use crate::scrub::Scrubber;
use crate::wait::Waiters;
use lu_audit::{Data, Event, Writer};
use lu_auth::{LoginThrottle, ThrottleSettings};
use lu_config::{ArtifactsConfig, ControllerConfig, ScopeConfig};
use lu_proto::{valid_group, JobId, NodeName, NodeStats, StuckPolicy, MAX_GROUP_RUNNING};
use lu_staging::ArtifactDir;
use lu_store::Store;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

type PendingEvents = Vec<serde_json::Value>;

const HEALTH_PROBE_TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct HealthCache {
    ok: bool,
    checked_at: Option<Instant>,
}

/// A single-flight, short-TTL cache for the unauthenticated health endpoint's DB probe. Without it
/// an anonymous client could keep every `spawn_blocking` thread and the SQLite mutex busy.
pub struct HealthProbe {
    cache: Mutex<HealthCache>,
    refresh: tokio::sync::Mutex<()>,
}

impl HealthProbe {
    fn new() -> Self {
        Self {
            cache: Mutex::new(HealthCache::default()),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    fn cached(&self) -> Option<bool> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match cache.checked_at {
            Some(at) if at.elapsed() < HEALTH_PROBE_TTL => Some(cache.ok),
            _ => None,
        }
    }

    /// Returns the last known DB state, refreshing it at most once per TTL. Concurrent callers
    /// wait for the in-flight probe rather than starting another blocking query.
    pub async fn db_ok(&self, store: &Arc<dyn Store>) -> bool {
        if let Some(ok) = self.cached() {
            return ok;
        }
        let _guard = self.refresh.lock().await;
        if let Some(ok) = self.cached() {
            return ok;
        }
        let store = Arc::clone(store);
        let ok = tokio::task::spawn_blocking(move || store.list_jobs(None, 1, None).is_ok())
            .await
            .unwrap_or(false);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.ok = ok;
        cache.checked_at = Some(Instant::now());
        ok
    }
}

/// Draft retention and inventory policy from `[drafts]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DraftPolicy {
    pub ttl_secs: u64,
    pub max_total: u64,
    pub max_per_source: u64,
}

impl Default for DraftPolicy {
    fn default() -> Self {
        Self {
            ttl_secs: 7 * 24 * 60 * 60,
            max_total: 10_000,
            max_per_source: 1_000,
        }
    }
}

pub struct AppState {
    pub store: Arc<dyn Store>,
    /// Shared, rate-limited DB probe for `/v1/health`.
    pub health: HealthProbe,
    /// Bounds concurrent mirror syncs and base packs, and their per-principal rates.
    pub sync_slots: Arc<Semaphore>,
    pub pack_slots: Arc<Semaphore>,
    pub sync_limiter: Arc<TokenLimiter>,
    pub pack_limiter: Arc<TokenLimiter>,
    /// The SQLite file, for size reporting and diagnostics.
    pub db_path: PathBuf,
    pub started: Instant,
    pub recovered_on_boot: AtomicU64,
    /// Jobs completed `done` with an empty diff and no `allow_empty` opt-out.
    pub no_changes_total: AtomicU64,
    /// Jobs the agent stopped because they were stuck, since this controller process started.
    pub stuck_stopped_cancel_total: AtomicU64,
    pub stuck_stopped_retry_total: AtomicU64,
    /// Jobs completed `done` with unmet expectations in suspect mode.
    pub expect_failed_total: AtomicU64,
    pub auth: AuthConfig,
    pub throttle: LoginThrottle,
    pub waiters: Arc<Waiters>,
    /// The latest host and job stats from each node's heartbeat. Deliberately in memory only.
    pub stats: Mutex<HashMap<NodeName, NodeStats>>,
    /// Operator clean requests waiting for the node's next poll. In memory only.
    pub pending_clean: Mutex<HashMap<NodeName, PendingEvents>>,
    pub artifacts_dir: PathBuf,
    /// Redacts secrets from artifacts before they are stored.
    pub scrubber: Scrubber,
    /// Ordered base providers; the first one that knows the repository wins.
    pub repo_providers: Vec<Arc<dyn RepoProvider>>,
    /// Cap applied when generating a base pack.
    pub max_pack_bytes: usize,
    pub mirror: Arc<Mirror>,
    pub lease_secs: u64,
    pub recovery_grace_secs: u64,
    pub provider_cooldown_secs: u64,
    pub max_retries: u32,
    pub lease_ms: u64,
    pub recovery_grace_ms: u64,
    pub cordon_ms: u64,
    /// How long a dependent blocked on a failed predecessor waits before it expires.
    pub blocked_dependent_ttl_secs: u64,
    /// Configured group caps, also installed in the store so dispatch enforces them atomically.
    /// Mutable at runtime so an operator batch can cap its own concurrency group.
    group_limits: Mutex<HashMap<String, usize>>,
    /// Job-scoped follow-up policy and its safe defaults.
    pub scope: ScopeConfig,
    /// Recent `submit_child` timestamps per job, for the per-minute spawn budget.
    scope_spawns: Mutex<HashMap<JobId, VecDeque<Instant>>>,
    /// `lu_scope_requests_total{route,result}` values.
    scope_requests: Mutex<HashMap<(String, String), u64>>,
    /// `lu_scope_limit_exceeded_total{limit}` values.
    scope_limits: Mutex<HashMap<String, u64>>,
    /// Signed webhook intake. `None` means every `/v1/intake/*` route is absent.
    pub intake: Option<Arc<crate::intake::Intake>>,
    /// Draft retention and inventory limits.
    pub drafts: DraftPolicy,
    /// `lu_intake_total{source,result}` values.
    intake_total: Mutex<HashMap<(String, String), u64>>,
    /// Audit writer, set once at startup when `[audit] path` is configured.
    audit: OnceLock<Writer>,
    /// Enqueue/write failures, shared with the writer task and exposed in metrics and status.
    pub audit_write_errors: Arc<AtomicU64>,
    /// Controller-only publish settings, set once at startup from `[publish]`.
    publish: OnceLock<Arc<lu_config::PublishConfig>>,
    /// Draft ids an operator explicitly approved in this process. Operator-submitted jobs need no
    /// entry: they are approved by construction.
    approved_drafts: Mutex<HashSet<JobId>>,
    /// `lu_publish_total{target,result}` values since process start.
    publish_total: Mutex<HashMap<(String, String), u64>>,
    /// Successful-push tickets in the last hour per publish remote. Reservations are released when
    /// a push fails, so only successful pushes consume the configured budget.
    publish_pushes: Mutex<HashMap<String, VecDeque<(Instant, u64)>>>,
    /// Monotonic ticket ids for publish push reservations.
    publish_ticket: AtomicU64,
}

impl AppState {
    /// Builds state with the default built-in scrub rules.
    pub fn from_config(cfg: &ControllerConfig, store: Arc<dyn Store>) -> Result<Self, String> {
        Self::from_config_with_scope(cfg, store, None, None)
    }

    /// Builds state with `[artifacts]` scrubbing configured by the top-level config.
    pub fn from_config_with_artifacts(
        cfg: &ControllerConfig,
        store: Arc<dyn Store>,
        artifacts: Option<&ArtifactsConfig>,
    ) -> Result<Self, String> {
        Self::from_config_with_scope(cfg, store, artifacts, None)
    }

    /// Builds state with top-level `[scope]` settings. Existing callers get the disabled default.
    pub fn from_config_with_scope(
        cfg: &ControllerConfig,
        store: Arc<dyn Store>,
        artifacts: Option<&ArtifactsConfig>,
        scope: Option<&ScopeConfig>,
    ) -> Result<Self, String> {
        Self::from_config_with_all(cfg, store, artifacts, scope, None, None, None)
    }

    /// Builds state with all top-level settings used by the service: scope, drafts and intake.
    pub fn from_config_with_all(
        cfg: &ControllerConfig,
        store: Arc<dyn Store>,
        artifacts: Option<&ArtifactsConfig>,
        scope: Option<&ScopeConfig>,
        drafts: Option<&lu_config::DraftsConfig>,
        intake: Option<lu_config::IntakeConfig>,
        mirror: Option<&lu_config::MirrorConfig>,
    ) -> Result<Self, String> {
        let drafts = match drafts {
            Some(config) => DraftPolicy {
                ttl_secs: lu_config::parse_duration_secs_public(&config.ttl)
                    .ok_or_else(|| "invalid [drafts] ttl".to_owned())?,
                max_total: config.max_drafts,
                max_per_source: config.max_drafts_per_source,
            },
            None => DraftPolicy::default(),
        };
        let intake = match intake {
            Some(config) => Some(Arc::new(crate::intake::Intake::from_config(&config)?)),
            None => None,
        };
        let scrubber = Scrubber::from_config(artifacts)?;
        let auth = AuthConfig::from_controller(cfg)?;
        let throttle = LoginThrottle::new(ThrottleSettings {
            max_failures: 10,
            window: Duration::from_secs(60),
        });
        // Ensure artifacts root exists? config validation requires absolute paths; runtime may need dir.
        // We'll create on demand via ArtifactDir usage where needed; but ArtifactDir::new requires existing real dir.
        // Better to ensure it exists now.
        std::fs::create_dir_all(&cfg.artifacts_dir)
            .map_err(|e| format!("cannot create artifacts_dir {}: {e}", cfg.artifacts_dir))?;
        // set permissions 0700 best-effort on unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &cfg.artifacts_dir,
                std::fs::Permissions::from_mode(0o700),
            );
        }
        let lease_secs = cfg.lease_secs;
        let mut repo_providers: Vec<Arc<dyn RepoProvider>> = Vec::new();
        if !cfg.local_repo_roots.is_empty() {
            repo_providers.push(Arc::new(LocalPathProvider::new(&cfg.local_repo_roots)?));
        }
        let mirror = Arc::new(Mirror::from_config_with_credentials(cfg, mirror)?);
        // A mirrored repository serves its pinned base from the mirror, after any local path root.
        repo_providers.push(mirror.clone());
        let group_limits: HashMap<String, usize> = cfg
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.max_running))
            .collect();
        store
            .set_group_limits(group_limits.clone())
            .map_err(|e| format!("cannot install group limits: {e:?}"))?;
        Ok(Self {
            store,
            health: HealthProbe::new(),
            sync_slots: Arc::new(Semaphore::new(SYNC_CONCURRENCY)),
            pack_slots: Arc::new(Semaphore::new(PACK_CONCURRENCY)),
            sync_limiter: Arc::new(TokenLimiter::new()),
            pack_limiter: Arc::new(TokenLimiter::new()),
            db_path: PathBuf::from(&cfg.db),
            started: Instant::now(),
            recovered_on_boot: AtomicU64::new(0),
            no_changes_total: AtomicU64::new(0),
            expect_failed_total: AtomicU64::new(0),
            stuck_stopped_cancel_total: AtomicU64::new(0),
            stuck_stopped_retry_total: AtomicU64::new(0),
            auth,
            throttle,
            waiters: Arc::new(Waiters::new()),
            stats: Mutex::new(HashMap::new()),
            pending_clean: Mutex::new(HashMap::new()),
            artifacts_dir: PathBuf::from(&cfg.artifacts_dir),
            scrubber,
            repo_providers,
            max_pack_bytes: lu_staging::PACK_CAP_BYTES,
            mirror,
            lease_secs,
            recovery_grace_secs: cfg.recovery_grace_secs,
            provider_cooldown_secs: cfg.provider_cooldown_secs,
            max_retries: cfg.max_retries,
            lease_ms: lease_secs.saturating_mul(1000),
            recovery_grace_ms: cfg.recovery_grace_secs.saturating_mul(1000),
            cordon_ms: cfg.provider_cooldown_secs.saturating_mul(1000),
            blocked_dependent_ttl_secs: cfg.blocked_dependent_ttl_secs(),
            group_limits: Mutex::new(group_limits),
            scope: scope.copied().unwrap_or_default(),
            scope_spawns: Mutex::new(HashMap::new()),
            scope_requests: Mutex::new(HashMap::new()),
            scope_limits: Mutex::new(HashMap::new()),
            intake,
            drafts,
            intake_total: Mutex::new(HashMap::new()),
            audit: OnceLock::new(),
            audit_write_errors: Arc::new(AtomicU64::new(0)),
            publish: OnceLock::new(),
            approved_drafts: Mutex::new(HashSet::new()),
            publish_total: Mutex::new(HashMap::new()),
            publish_pushes: Mutex::new(HashMap::new()),
            publish_ticket: AtomicU64::new(0),
        })
    }

    /// A point-in-time copy of the group caps, for views and metrics.
    pub fn group_limits(&self) -> HashMap<String, usize> {
        self.group_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Changes one runtime group cap in both the store and the cached view. The store is updated
    /// first, so a failed write leaves dispatch and reporting on the same previous value.
    pub fn set_group_limit(&self, group: &str, max_running: usize) -> Result<(), String> {
        if !valid_group(group) {
            return Err("group must be 1 to 64 chars from A-Z a-z 0-9 . _ -".to_owned());
        }
        if !(1..=MAX_GROUP_RUNNING).contains(&max_running) {
            return Err(format!(
                "max_running must be 1 to {MAX_GROUP_RUNNING} (got {max_running})"
            ));
        }
        let mut cached = self
            .group_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut limits = cached.clone();
        limits.insert(group.to_owned(), max_running);
        self.store
            .set_group_limits(limits.clone())
            .map_err(|error| format!("cannot install group limits: {error:?}"))?;
        *cached = limits;
        Ok(())
    }

    /// Counts one intake outcome. Source names come from config; results are fixed labels.
    pub fn record_intake(&self, source: &str, result: &str) {
        let mut totals = self
            .intake_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *totals
            .entry((source.to_owned(), result.to_owned()))
            .or_insert(0) += 1;
        drop(totals);
        self.record_audit(
            lu_audit::Event::Intake,
            None,
            None,
            source,
            lu_audit::Data::new()
                .text(lu_audit::Field::Source, source)
                .text(lu_audit::Field::State, result),
        );
    }

    /// Snapshot of `lu_intake_total` values.
    pub fn intake_totals(&self) -> Vec<(String, String, u64)> {
        let totals = self
            .intake_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<_> = totals
            .iter()
            .map(|((source, result), count)| (source.clone(), result.clone(), *count))
            .collect();
        out.sort();
        out
    }

    /// Installs the audit writer. A second call is ignored; auditing is configured once.
    pub fn set_audit(&self, writer: Writer) {
        let _ = self.audit.set(writer);
    }

    /// Installs `[publish]`, including the only copy of each push token path.
    pub fn set_publish(&self, config: lu_config::PublishConfig) {
        let _ = self.publish.set(Arc::new(config));
    }

    pub fn publish_config(&self) -> Option<&Arc<lu_config::PublishConfig>> {
        self.publish.get()
    }

    /// Marks a draft as explicitly approved by an operator.
    pub fn mark_approved(&self, id: &JobId) {
        self.approved_drafts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone());
    }

    /// Whether an operator submitted the job (or explicitly approved its draft). A job delivered
    /// by an intake source is only approved when an operator approved the draft it became.
    pub fn operator_approved(&self, job: &lu_store::JobRecord) -> bool {
        (job.spec.origin.is_none() && !job.spec.draft)
            || self
                .approved_drafts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&job.id)
    }

    /// Counts one publish attempt outcome for `/metrics`.
    pub fn record_publish_total(&self, target: &str, result: &str) {
        let mut totals = self
            .publish_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *totals
            .entry((target.to_owned(), result.to_owned()))
            .or_insert(0) += 1;
    }

    pub fn publish_totals(&self) -> Vec<(String, String, u64)> {
        let totals = self
            .publish_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<_> = totals
            .iter()
            .map(|((target, result), count)| (target.clone(), result.clone(), *count))
            .collect();
        out.sort();
        out
    }

    /// Reserves one slot in a remote's rolling-hour push budget. The returned ticket must be
    /// released when the push does not succeed. `Err` carries seconds until the oldest slot expires.
    pub fn reserve_publish_push(&self, remote: &str, max: u32) -> Result<u64, u64> {
        let mut pushes = self
            .publish_pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let queue = pushes.entry(remote.to_owned()).or_default();
        let now = Instant::now();
        let window = Duration::from_secs(3600);
        while queue
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= window)
        {
            queue.pop_front();
        }
        if queue.len() >= max as usize {
            let retry_after = queue
                .front()
                .map(|(at, _)| window.saturating_sub(at.elapsed()).as_secs().max(1))
                .unwrap_or(1);
            return Err(retry_after);
        }
        let ticket = self.publish_ticket.fetch_add(1, Ordering::Relaxed);
        queue.push_back((now, ticket));
        Ok(ticket)
    }

    /// Releases a reservation after a failed push, so only successful pushes consume the budget.
    pub fn release_publish_push(&self, remote: &str, ticket: u64) {
        let mut pushes = self
            .publish_pushes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(queue) = pushes.get_mut(remote) {
            queue.retain(|(_, id)| *id != ticket);
        }
    }

    /// Records an audit event when enabled. No-op (and allocation-free) when off.
    pub fn record_audit(
        &self,
        event: Event,
        job: Option<&str>,
        node: Option<&str>,
        actor: &str,
        data: Data,
    ) {
        if let Some(writer) = self.audit.get() {
            writer.record(event, job, node, actor, data);
        }
    }

    pub fn audit_write_errors(&self) -> u64 {
        self.audit_write_errors.load(Ordering::Relaxed)
    }

    /// Atomically consumes one slot in a job's spawning budget. The window slides, so a burst
    /// spread across two windows cannot double the allowance. Returns the consumed token so a
    /// failed store write can refund it.
    pub fn allow_scope_spawn(&self, job: &JobId) -> Option<Instant> {
        let budget = self.scope.max_spawn_per_minute;
        let mut map = self
            .scope_spawns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cutoff = Instant::now() - Duration::from_secs(60);
        for times in map.values_mut() {
            while times.front().is_some_and(|at| *at <= cutoff) {
                times.pop_front();
            }
        }
        map.retain(|_, times| !times.is_empty());
        let times = map.entry(job.clone()).or_default();
        if times.len() as u64 >= budget {
            return None;
        }
        let token = Instant::now();
        times.push_back(token);
        Some(token)
    }

    /// Returns a slot consumed by [`Self::allow_scope_spawn`] when the create it reserved failed.
    pub fn refund_scope_spawn(&self, job: &JobId, token: Instant) {
        let mut map = self
            .scope_spawns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(times) = map.get_mut(job) {
            if let Some(index) = times.iter().rposition(|at| *at == token) {
                times.remove(index);
            }
            if times.is_empty() {
                map.remove(job);
            }
        }
    }

    pub fn record_scope_request(&self, route: &str, result: &str) {
        let mut map = self
            .scope_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry((route.to_owned(), result.to_owned()))
            .or_default() += 1;
    }

    pub fn record_scope_limit(&self, limit: &'static str) {
        let mut map = self
            .scope_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry(limit.to_owned()).or_default() += 1;
    }

    pub fn scope_request_counts(&self) -> Vec<((String, String), u64)> {
        let map = self
            .scope_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        rows.sort();
        rows
    }

    pub fn scope_limit_counts(&self) -> Vec<(String, u64)> {
        let map = self
            .scope_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        rows.sort();
        rows
    }

    pub fn artifact_dir(&self) -> Result<ArtifactDir, String> {
        ArtifactDir::new(&self.artifacts_dir).map_err(|e| e.to_string())
    }

    pub fn set_recovered_on_boot(&self, count: u64) {
        self.recovered_on_boot.store(count, Ordering::Relaxed);
    }

    pub fn recovered_on_boot(&self) -> u64 {
        self.recovered_on_boot.load(Ordering::Relaxed)
    }

    pub fn count_no_changes(&self) {
        self.no_changes_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn no_changes_total(&self) -> u64 {
        self.no_changes_total.load(Ordering::Relaxed)
    }

    pub fn count_expect_failed(&self) {
        self.expect_failed_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn expect_failed_total(&self) -> u64 {
        self.expect_failed_total.load(Ordering::Relaxed)
    }

    pub fn count_stuck_stopped(&self, mode: Option<StuckPolicy>) {
        let Some(mode) = mode else {
            return;
        };
        match mode {
            StuckPolicy::Cancel => {
                self.stuck_stopped_cancel_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            StuckPolicy::Retry => {
                self.stuck_stopped_retry_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            StuckPolicy::None => {}
        }
    }

    pub fn stuck_stopped_total(&self, mode: StuckPolicy) -> u64 {
        match mode {
            StuckPolicy::Cancel => self.stuck_stopped_cancel_total.load(Ordering::Relaxed),
            StuckPolicy::Retry => self.stuck_stopped_retry_total.load(Ordering::Relaxed),
            StuckPolicy::None => 0,
        }
    }
}
