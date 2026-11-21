//! Bounds for controller operations that are cheap to ask for but expensive to serve.
//!
//! Repository syncs and base-pack generation do real git work. A configured principal may only
//! start so many per window, and only so many may run at once. Keys are the authenticated operator
//! or node name, never the bearer token itself.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Only one mirror fetch per controller.
pub const SYNC_CONCURRENCY: usize = 1;
/// Base packs are read-only, so a few may be built in parallel.
pub const PACK_CONCURRENCY: usize = 4;
pub const SYNC_PER_TOKEN: usize = 6;
pub const PACK_PER_TOKEN: usize = 30;
pub const TOKEN_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct Window {
    reset_at: Instant,
    used: usize,
}

/// A fixed-window counter per authenticated principal. All keys come from the configured
/// operators or agents, so the map cannot grow from unauthenticated input.
#[derive(Default)]
pub struct TokenLimiter {
    windows: Mutex<HashMap<String, Window>>,
}

impl TokenLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes one unit of `key`'s budget, or returns seconds until the next window.
    pub fn allow(&self, key: &str, limit: usize) -> Result<(), u64> {
        self.check(key, limit, Instant::now())
    }

    /// The clock is injected so window rollover can be tested without sleeping.
    pub fn check(&self, key: &str, limit: usize, now: Instant) -> Result<(), u64> {
        let mut windows = self.lock();
        windows.retain(|_, window| window.reset_at > now);
        match windows.get_mut(key) {
            Some(window) => {
                if window.used >= limit {
                    let retry_after = window
                        .reset_at
                        .saturating_duration_since(now)
                        .as_secs()
                        .max(1);
                    return Err(retry_after);
                }
                window.used += 1;
            }
            None => {
                windows.insert(
                    key.to_owned(),
                    Window {
                        reset_at: now + TOKEN_WINDOW,
                        used: 1,
                    },
                );
            }
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Window>> {
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_limits_one_principal_and_resets_with_the_clock() {
        let limiter = TokenLimiter::new();
        let start = Instant::now();
        assert!(limiter.check("ops", 2, start).is_ok());
        assert!(limiter.check("ops", 2, start).is_ok());
        let retry = limiter.check("ops", 2, start).unwrap_err();
        assert!((1..=TOKEN_WINDOW.as_secs()).contains(&retry), "{retry}");

        assert!(limiter.check("other", 2, start).is_ok());
        assert!(limiter
            .check("ops", 2, start + TOKEN_WINDOW + Duration::from_millis(1))
            .is_ok());
    }
}
