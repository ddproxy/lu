use crate::auth::AuthConfig;
use crate::limits::{TokenLimiter, PACK_CONCURRENCY, SYNC_CONCURRENCY};
use crate::mirror::Mirror;
use crate::repo::{LocalPathProvider, RepoProvider};
use crate::wait::Waiters;
use lu_auth::{LoginThrottle, ThrottleSettings};
use lu_config::ControllerConfig;
use lu_proto::{NodeName, NodeStats, StuckPolicy};
use lu_staging::ArtifactDir;
use lu_store::Store;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

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
    pub artifacts_dir: PathBuf,
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
    /// Configured group caps, also installed in the store so dispatch enforces them atomically.
    pub group_limits: HashMap<String, usize>,
}

impl AppState {
    pub fn from_config(cfg: &ControllerConfig, store: Arc<dyn Store>) -> Result<Self, String> {
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
        let mirror = Arc::new(Mirror::from_config(cfg)?);
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
            artifacts_dir: PathBuf::from(&cfg.artifacts_dir),
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
            group_limits,
        })
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
