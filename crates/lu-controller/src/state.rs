use crate::auth::AuthConfig;
use crate::wait::Waiters;
use lu_auth::{LoginThrottle, ThrottleSettings};
use lu_config::ControllerConfig;
use lu_proto::{NodeName, NodeStats};
use lu_staging::ArtifactDir;
use lu_store::Store;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct AppState {
    pub store: Arc<dyn Store>,
    /// The SQLite file, for size reporting and diagnostics.
    pub db_path: PathBuf,
    pub started: Instant,
    pub recovered_on_boot: AtomicU64,
    pub auth: AuthConfig,
    pub throttle: LoginThrottle,
    pub waiters: Arc<Waiters>,
    /// The latest host and job stats from each node's heartbeat. Deliberately in memory only.
    pub stats: Mutex<HashMap<NodeName, NodeStats>>,
    pub artifacts_dir: PathBuf,
    pub lease_secs: u64,
    pub recovery_grace_secs: u64,
    pub provider_cooldown_secs: u64,
    pub max_retries: u32,
    pub lease_ms: u64,
    pub recovery_grace_ms: u64,
    pub cordon_ms: u64,
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
        Ok(Self {
            store,
            db_path: PathBuf::from(&cfg.db),
            started: Instant::now(),
            recovered_on_boot: AtomicU64::new(0),
            auth,
            throttle,
            waiters: Arc::new(Waiters::new()),
            stats: Mutex::new(HashMap::new()),
            artifacts_dir: PathBuf::from(&cfg.artifacts_dir),
            lease_secs,
            recovery_grace_secs: cfg.recovery_grace_secs,
            provider_cooldown_secs: cfg.provider_cooldown_secs,
            max_retries: cfg.max_retries,
            lease_ms: lease_secs.saturating_mul(1000),
            recovery_grace_ms: cfg.recovery_grace_secs.saturating_mul(1000),
            cordon_ms: cfg.provider_cooldown_secs.saturating_mul(1000),
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
}
