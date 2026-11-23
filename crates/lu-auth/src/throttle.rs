use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Addresses tracked at once. Past this, an expired entry is evicted first, so a flood of
/// addresses cannot grow the table without bound.
const MAX_TRACKED: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleSettings {
    /// Wrong-token failures allowed in one window.
    pub max_failures: u32,
    /// Requests with no token at all allowed in one window. Deliberately more generous: a client
    /// that loses a session keeps polling without a token, and must not lock out other clients
    /// that share its source address.
    pub missing_token_max_failures: u32,
    pub window: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Bucket {
    /// A token was presented but did not match.
    Token,
    /// No token was presented.
    MissingToken,
}

type Seen = HashMap<(IpAddr, String, Bucket), (u32, Instant)>;

/// Counts failed token checks per address, scope and failure kind. A blocked key gets the time
/// left in its window. Tokenless requests never fill the wrong-token bucket.
pub struct LoginThrottle {
    settings: ThrottleSettings,
    seen: Mutex<Seen>,
}

/// An IPv4 address is its own key. An IPv6 address is keyed by its /64, so a host that rotates
/// through its prefix shares one count. A mapped IPv4 address counts as the IPv4 address.
fn throttle_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut octets = v6.octets();
            octets[8..].fill(0);
            IpAddr::V6(octets.into())
        }
    }
}

impl LoginThrottle {
    pub fn new(settings: ThrottleSettings) -> Self {
        Self {
            settings,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn max_failures(&self, bucket: Bucket) -> u32 {
        match bucket {
            Bucket::Token => self.settings.max_failures,
            Bucket::MissingToken => self.settings.missing_token_max_failures,
        }
    }

    fn check(&self, ip: IpAddr, scope: &str, now: Instant, bucket: Bucket) -> Option<Duration> {
        let key = (throttle_key(ip), scope.to_owned(), bucket);
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let (count, since) = seen.get(&key)?;
        let elapsed = now.saturating_duration_since(*since);
        if elapsed >= self.settings.window {
            return None;
        }
        (*count >= self.max_failures(bucket)).then(|| self.settings.window - elapsed)
    }

    /// How long a wrong token from `ip` must wait in `scope`, or `None` when it may try.
    pub fn blocked(&self, ip: IpAddr, scope: &str, now: Instant) -> Option<Duration> {
        self.check(ip, scope, now, Bucket::Token)
    }

    /// How long a tokenless request from `ip` must wait in `scope`.
    pub fn blocked_missing(&self, ip: IpAddr, scope: &str, now: Instant) -> Option<Duration> {
        self.check(ip, scope, now, Bucket::MissingToken)
    }

    fn record(&self, ip: IpAddr, scope: &str, now: Instant, bucket: Bucket) -> u32 {
        let key = (throttle_key(ip), scope.to_owned(), bucket);
        let window = self.settings.window;
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= MAX_TRACKED && !seen.contains_key(&key) {
            // Evict the least harmful entry: expired first, then unblocked, then least failed.
            let victim = seen
                .iter()
                .min_by_key(|((_, _, entry_bucket), (count, since))| {
                    let expired = now.saturating_duration_since(*since) >= window;
                    let entry_max = self.max_failures(*entry_bucket);
                    (!expired, *count >= entry_max, *count, *since)
                })
                .map(|(entry_key, _)| entry_key.clone());
            if let Some(victim) = victim {
                seen.remove(&victim);
            }
        }
        let entry = seen.entry(key).or_insert((0, now));
        if now.saturating_duration_since(entry.1) >= window {
            *entry = (0, now);
        }
        entry.0 = entry.0.saturating_add(1);
        entry.0
    }

    /// Records a wrong-token failure and returns how many the address has in the current window.
    pub fn fail(&self, ip: IpAddr, scope: &str, now: Instant) -> u32 {
        self.record(ip, scope, now, Bucket::Token)
    }

    /// Records a tokenless request in the separate, more generous bucket.
    pub fn fail_missing(&self, ip: IpAddr, scope: &str, now: Instant) -> u32 {
        self.record(ip, scope, now, Bucket::MissingToken)
    }

    // There is deliberately no success reset: see the test of the same intent below.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn throttle() -> LoginThrottle {
        LoginThrottle::new(ThrottleSettings {
            max_failures: 3,
            missing_token_max_failures: 6,
            window: Duration::from_secs(60),
        })
    }

    #[test]
    fn an_address_is_blocked_after_max_failures_with_the_time_left() {
        let throttle = throttle();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        assert_eq!(throttle.fail(ip, "operator", t0), 1);
        assert_eq!(throttle.fail(ip, "operator", t0), 2);
        assert!(
            throttle.blocked(ip, "operator", t0).is_none(),
            "two failures are allowed"
        );
        assert_eq!(throttle.fail(ip, "operator", t0), 3);
        let wait = throttle
            .blocked(ip, "operator", t0 + Duration::from_secs(20))
            .expect("blocked");
        assert_eq!(wait, Duration::from_secs(40));
    }

    #[test]
    fn the_block_lifts_when_the_window_passes() {
        let throttle = throttle();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..3 {
            throttle.fail(ip, "operator", t0);
        }
        assert!(throttle
            .blocked(ip, "operator", t0 + Duration::from_secs(60))
            .is_none());
        assert_eq!(
            throttle.fail(ip, "operator", t0 + Duration::from_secs(61)),
            1,
            "the count restarts"
        );
    }

    #[test]
    fn the_count_is_never_cleared_by_another_login_only_by_its_window() {
        // A caller holding one valid credential must not be able to reset the count between
        // guesses at another, so the throttle has no success reset.
        let throttle = throttle();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..10 {
            throttle.fail(ip, "operator", t0);
        }
        assert!(throttle.blocked(ip, "operator", t0).is_some());
        assert!(throttle
            .blocked(ip, "operator", t0 + Duration::from_secs(30))
            .is_some());
        assert!(throttle
            .blocked(ip, "operator", t0 + Duration::from_secs(61))
            .is_none());
    }

    #[test]
    fn a_count_at_the_maximum_saturates_instead_of_overflowing() {
        let throttle = throttle();
        let ip: IpAddr = "192.0.2.8".parse().unwrap();
        let t0 = Instant::now();
        throttle
            .seen
            .lock()
            .unwrap()
            .insert((ip, "operator".to_owned(), Bucket::Token), (u32::MAX, t0));
        assert_eq!(throttle.fail(ip, "operator", t0), u32::MAX);
    }

    #[test]
    fn addresses_are_counted_separately_and_ipv6_by_slash_64() {
        let throttle = throttle();
        let t0 = Instant::now();
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let same_prefix: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other: IpAddr = "192.0.2.1".parse().unwrap();
        for _ in 0..2 {
            throttle.fail(a, "operator", t0);
        }
        throttle.fail(same_prefix, "operator", t0);
        assert!(
            throttle.blocked(a, "operator", t0).is_some(),
            "the /64 shares one count"
        );
        assert!(throttle.blocked(other, "operator", t0).is_none());
    }

    #[test]
    fn the_table_stays_bounded_under_an_address_flood() {
        let throttle = throttle();
        let t0 = Instant::now();
        for i in 0..(MAX_TRACKED as u32 + 500) {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000 + i));
            throttle.fail(ip, "operator", t0);
        }
        assert_eq!(throttle.tracked(), MAX_TRACKED);
    }

    #[test]
    fn scopes_are_throttled_independently() {
        let throttle = throttle();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..3 {
            throttle.fail(ip, "agent", t0);
        }
        assert!(throttle.blocked(ip, "agent", t0).is_some());
        assert!(throttle.blocked(ip, "operator", t0).is_none());
    }

    #[test]
    fn tokenless_requests_use_a_separate_generous_bucket() {
        let throttle = throttle();
        let ip: IpAddr = "192.0.2.9".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..3 {
            throttle.fail(ip, "operator", t0);
        }
        assert!(throttle.blocked(ip, "operator", t0).is_some());
        assert!(throttle.blocked_missing(ip, "operator", t0).is_none());

        let other: IpAddr = "192.0.2.10".parse().unwrap();
        for _ in 0..5 {
            throttle.fail_missing(other, "operator", t0);
        }
        assert!(throttle.blocked_missing(other, "operator", t0).is_none());
        assert_eq!(throttle.fail_missing(other, "operator", t0), 6);
        assert!(throttle.blocked_missing(other, "operator", t0).is_some());
        assert!(
            throttle.blocked(other, "operator", t0).is_none(),
            "tokenless traffic never fills the wrong-token bucket"
        );
    }
}
