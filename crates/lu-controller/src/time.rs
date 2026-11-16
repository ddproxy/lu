use lu_store::Now;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> Now {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    let wall_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mono_ms = start.elapsed().as_millis() as u64;
    Now { wall_unix, mono_ms }
}

pub fn background_interval_secs(lease_secs: u64) -> u64 {
    (lease_secs / 3).max(1)
}
