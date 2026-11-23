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
        for o in &cfg.operators {
            if agents.contains_key(&o.token_sha256) || operators.contains_key(&o.token_sha256) {
                return Err("duplicate token hash across scopes".to_owned());
            }
            operators.insert(o.token_sha256.clone(), o.name.clone());
        }
        Ok(Self { agents, operators })
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
