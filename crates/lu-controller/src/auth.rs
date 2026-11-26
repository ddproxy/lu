use crate::error::ApiError;
use axum::http::{header, HeaderMap};
use lu_auth::{bearer_token, constant_time_eq, sha256_hex, LoginThrottle};
use lu_proto::NodeName;
use lu_serve::PeerAddr;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub agents: HashMap<String, NodeName>,  // hash -> node name
    pub operators: HashMap<String, String>, // hash -> operator name
    /// Role, expiry and field policy per operator name. A name with no entry is a plain operator.
    pub profiles: HashMap<String, OperatorProfile>,
    /// Operator names allowed to set job schedule constraints and reload calendars. The map is
    /// keyed by name so callers that already hold the authenticated name need no token hash.
    operator_schedule: HashMap<String, bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorProfile {
    pub role: lu_config::OperatorRole,
    /// Unix seconds from which the token is refused.
    pub expires_at: Option<i64>,
    pub restrict_fields: bool,
}

impl AuthConfig {
    pub fn profile(&self, name: &str) -> Option<&OperatorProfile> {
        self.profiles.get(name)
    }

    /// True when this operator's submissions must not widen what a job may do.
    pub fn restricts_fields(&self, name: &str) -> bool {
        self.profile(name).is_some_and(|p| p.restrict_fields)
    }

    /// The operator a presented bearer token belongs to, whether or not it has expired.
    pub fn operator_for_token(&self, token: &str) -> Option<&str> {
        let presented = sha256_hex(token);
        let mut found = None;
        for (hash, name) in &self.operators {
            if constant_time_eq(presented.as_bytes(), hash.as_bytes()) && found.is_none() {
                found = Some(name.as_str());
            }
        }
        found
    }

    /// Whether a named operator may set schedule constraints or reload calendars. Default deny:
    /// an operator absent from the map has no permission.
    pub fn can_schedule(&self, name: &str) -> bool {
        self.operator_schedule.get(name).copied().unwrap_or(false)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl AuthConfig {
    pub fn from_controller(cfg: &lu_config::ControllerConfig) -> Result<Self, String> {
        let mut agents = HashMap::new();
        for a in &cfg.agents {
            let node = NodeName::parse(&a.name).map_err(|e| e.to_string())?;
            if agents.insert(a.token_sha256.clone(), node).is_some() {
                return Err("duplicate agent token hash".to_owned());
            }
        }
        let mut operators = HashMap::new();
        let mut profiles = HashMap::new();
        let mut operator_schedule = HashMap::new();
        for o in &cfg.operators {
            if agents.contains_key(&o.token_sha256) || operators.contains_key(&o.token_sha256) {
                return Err("duplicate token hash across scopes".to_owned());
            }
            operator_schedule.insert(o.name.clone(), o.schedule);
            operators.insert(o.token_sha256.clone(), o.name.clone());
            let expires_at = match &o.expires_at {
                Some(text) => Some(
                    lu_config::parse_utc_timestamp(text)
                        .ok_or_else(|| format!("operator {}: invalid expires_at", o.name))?,
                ),
                None => None,
            };
            profiles.insert(
                o.name.clone(),
                OperatorProfile {
                    role: o.role,
                    expires_at,
                    restrict_fields: o.restrict_fields,
                },
            );
        }
        Ok(Self {
            agents,
            operators,
            profiles,
            operator_schedule,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Principal {
    Operator(String),
    Agent(NodeName),
}

pub fn check_rate_limit(
    throttle: &LoginThrottle,
    ip: IpAddr,
    scope: &str,
    now: Instant,
) -> Result<(), ApiError> {
    if let Some(wait) = throttle.blocked(ip, scope, now) {
        return Err(ApiError::rate_limited(wait.as_secs().max(1)));
    }
    Ok(())
}

/// Tokenless requests have their own bucket so a client without a token cannot consume the
/// wrong-token budget shared by every client behind the same source address.
fn check_missing_rate_limit(
    throttle: &LoginThrottle,
    ip: IpAddr,
    scope: &str,
    now: Instant,
) -> Result<(), ApiError> {
    if let Some(wait) = throttle.blocked_missing(ip, scope, now) {
        return Err(ApiError::rate_limited(wait.as_secs().max(1)));
    }
    Ok(())
}

pub fn presented_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)
}

pub fn authenticate_operator(
    headers: &HeaderMap,
    peer: PeerAddr,
    auth: &AuthConfig,
    throttle: &LoginThrottle,
) -> Result<String, ApiError> {
    let now = Instant::now();
    check_rate_limit(throttle, peer.0.ip(), "operator", now)?;
    let token = match presented_token(headers) {
        Some(token) => token,
        None => {
            check_missing_rate_limit(throttle, peer.0.ip(), "operator", now)?;
            throttle.fail_missing(peer.0.ip(), "operator", now);
            return Err(ApiError::unauthorized());
        }
    };
    let presented = sha256_hex(token);
    let mut found: Option<String> = None;
    for (hash, name) in &auth.operators {
        if constant_time_eq(presented.as_bytes(), hash.as_bytes()) && found.is_none() {
            found = Some(name.clone());
        }
    }
    // An expired token is refused exactly like an unknown one.
    let found = found.filter(|name| {
        auth.profile(name)
            .and_then(|p| p.expires_at)
            .is_none_or(|expiry| unix_now() < expiry)
    });
    match found {
        Some(name) => Ok(name),
        None => {
            throttle.fail(peer.0.ip(), "operator", now);
            Err(ApiError::unauthorized())
        }
    }
}

pub fn authenticate_agent(
    headers: &HeaderMap,
    peer: PeerAddr,
    auth: &AuthConfig,
    throttle: &LoginThrottle,
) -> Result<NodeName, ApiError> {
    let now = Instant::now();
    check_rate_limit(throttle, peer.0.ip(), "agent", now)?;
    let token = match presented_token(headers) {
        Some(token) => token,
        None => {
            check_missing_rate_limit(throttle, peer.0.ip(), "agent", now)?;
            throttle.fail_missing(peer.0.ip(), "agent", now);
            return Err(ApiError::unauthorized());
        }
    };
    let presented = sha256_hex(token);
    let mut found: Option<NodeName> = None;
    for (hash, node) in &auth.agents {
        if constant_time_eq(presented.as_bytes(), hash.as_bytes()) && found.is_none() {
            found = Some(node.clone());
        }
    }
    match found {
        Some(node) => Ok(node),
        None => {
            throttle.fail(peer.0.ip(), "agent", now);
            Err(ApiError::unauthorized())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use axum::http::HeaderValue;
    use lu_auth::ThrottleSettings;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn config() -> (AuthConfig, String) {
        // Built here so no credential-shaped text is stored in the source tree.
        let token = ['o', 'p', 'e', 'r'].iter().collect::<String>();
        let auth = AuthConfig {
            agents: HashMap::new(),
            operators: HashMap::from([(sha256_hex(&token), "ops".to_owned())]),
            profiles: HashMap::new(),
            operator_schedule: HashMap::new(),
        };
        (auth, token)
    }

    fn peer() -> PeerAddr {
        PeerAddr(SocketAddr::from(([192, 0, 2, 1], 4321)))
    }

    fn throttle() -> LoginThrottle {
        LoginThrottle::new(ThrottleSettings {
            max_failures: 2,
            missing_token_max_failures: 4,
            window: Duration::from_secs(60),
        })
    }

    fn headers(token: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).expect("valid header"),
            );
        }
        headers
    }

    #[test]
    fn tokenless_requests_do_not_consume_the_wrong_token_budget() {
        let (auth, token) = config();
        let throttle = throttle();
        for _ in 0..3 {
            assert!(authenticate_operator(&headers(None), peer(), &auth, &throttle).is_err());
        }
        assert_eq!(
            authenticate_operator(&headers(Some(&token)), peer(), &auth, &throttle).unwrap(),
            "ops"
        );
    }

    #[test]
    fn a_full_missing_bucket_does_not_block_token_requests() {
        let (auth, token) = config();
        let throttle = throttle();
        for _ in 0..4 {
            assert_eq!(
                authenticate_operator(&headers(None), peer(), &auth, &throttle)
                    .unwrap_err()
                    .code,
                ErrorCode::Unauthorized
            );
        }
        let blocked = authenticate_operator(&headers(None), peer(), &auth, &throttle).unwrap_err();
        assert_eq!(blocked.code, ErrorCode::RateLimited);
        assert_eq!(
            authenticate_operator(&headers(Some(&token)), peer(), &auth, &throttle).unwrap(),
            "ops"
        );
    }

    #[test]
    fn forwarded_headers_do_not_change_the_login_throttle_key() {
        let (auth, token) = config();
        let throttle = throttle();
        let other_peer = PeerAddr(SocketAddr::from(([203, 0, 113, 9], 4321)));
        let other = ['o', 't', 'h', 'e', 'r'].iter().collect::<String>();
        let mut spoofed = headers(Some(&other));
        spoofed.insert(
            axum::http::HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("203.0.113.9"),
        );
        spoofed.insert(
            axum::http::HeaderName::from_static("forwarded"),
            HeaderValue::from_static("for=203.0.113.9"),
        );
        for _ in 0..2 {
            assert_eq!(
                authenticate_operator(&spoofed, peer(), &auth, &throttle)
                    .unwrap_err()
                    .code,
                ErrorCode::Unauthorized
            );
        }

        // The same headers from the real address named in them must not inherit the other
        // peer's failures, and the blocked peer cannot reset its bucket by changing headers.
        let mut allowed = headers(Some(&token));
        allowed.insert(
            axum::http::HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("203.0.113.9"),
        );
        assert_eq!(
            authenticate_operator(&allowed, other_peer, &auth, &throttle).unwrap(),
            "ops"
        );
        assert_eq!(
            authenticate_operator(&allowed, peer(), &auth, &throttle)
                .unwrap_err()
                .code,
            ErrorCode::RateLimited
        );
    }

    #[test]
    fn wrong_tokens_keep_the_original_budget() {
        let (auth, token) = config();
        let throttle = throttle();
        let other = ['o', 't', 'h', 'e', 'r'].iter().collect::<String>();
        for _ in 0..2 {
            assert_eq!(
                authenticate_operator(&headers(Some(&other)), peer(), &auth, &throttle)
                    .unwrap_err()
                    .code,
                ErrorCode::Unauthorized
            );
        }
        assert_eq!(
            authenticate_operator(&headers(Some(&token)), peer(), &auth, &throttle)
                .unwrap_err()
                .code,
            ErrorCode::RateLimited
        );
    }
}
