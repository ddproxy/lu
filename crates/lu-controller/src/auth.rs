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
    now: Instant,
) -> Result<(), ApiError> {
    if let Some(wait) = throttle.blocked(ip, now) {
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
    check_rate_limit(throttle, peer.0.ip(), now)?;
    let token = presented_token(headers).ok_or_else(|| {
        throttle.fail(peer.0.ip(), now);
        ApiError::unauthorized()
    })?;
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
            throttle.fail(peer.0.ip(), now);
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
    check_rate_limit(throttle, peer.0.ip(), now)?;
    let token = presented_token(headers).ok_or_else(|| {
        throttle.fail(peer.0.ip(), now);
        ApiError::unauthorized()
    })?;
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
            throttle.fail(peer.0.ip(), now);
            Err(ApiError::unauthorized())
        }
    }
}
