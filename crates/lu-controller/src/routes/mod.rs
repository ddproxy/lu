pub mod agent;
pub mod file_body;
pub mod health;
pub mod metrics;
pub mod operator;
pub mod status;

use crate::error::{ApiError, ApiResult};
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;

pub const MAX_JSON_BODY: usize = 1024 * 1024;
pub const MAX_LOG_CHUNK_BODY: usize = lu_proto::MAX_LOG_CHUNK_BYTES + 1024; // small slack
pub const MAX_DIFF_BODY: usize = (10 * 1024 * 1024) + 4096;
pub const MAX_REPORT_BODY: usize = (1024 * 1024) + 4096;

pub fn json_ok<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_string(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(_) => ApiError::internal().into_response(),
    }
}

pub fn json_value_ok(status: StatusCode, value: serde_json::Value) -> Response {
    json_ok(status, &value)
}

pub async fn read_json<T: DeserializeOwned>(req: Request, limit: usize) -> ApiResult<T> {
    // Enforce content-type? not specified, but strict JSON parsing required.
    let bytes = to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| ApiError::too_large("request body too large"))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::bad_request("invalid request body"))
}

pub async fn read_raw_limited(req: Request, limit: usize) -> ApiResult<axum::body::Bytes> {
    to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| ApiError::too_large("request body too large"))
}

pub fn store_err(e: lu_store::StoreError) -> ApiError {
    use lu_store::StoreError;
    match e {
        StoreError::NotFound => ApiError::not_found("not found"),
        StoreError::Conflict { length } => {
            let mut err = ApiError::conflict("conflict");
            if let Some(l) = length {
                err = err.with_log_length(l);
            }
            err
        }
        StoreError::StaleEpoch => ApiError::stale_epoch(),
        StoreError::InvalidDependency(id) => {
            ApiError::unprocessable(format!("unknown dependency {id}"))
        }
        StoreError::DeadDependency(id) => {
            ApiError::unprocessable(format!("dependency {id} already failed or was cancelled"))
        }
        StoreError::DependencyCycle => ApiError::bad_request("dependencies would form a cycle"),
        StoreError::InvalidSpec(message) => ApiError::unprocessable(message),
        StoreError::Internal(_) => ApiError::internal(),
    }
}

pub fn job_state_str(s: lu_proto::JobState) -> &'static str {
    s.as_str()
}

pub fn failure_class_str(class: lu_proto::FailureClass) -> &'static str {
    match class {
        lu_proto::FailureClass::Node => "node",
        lu_proto::FailureClass::Provider => "provider",
        lu_proto::FailureClass::Execution => "execution",
        lu_proto::FailureClass::Dependency => "dependency",
        lu_proto::FailureClass::Conflict => "conflict",
    }
}

pub fn parse_job_state(s: &str) -> Option<lu_proto::JobState> {
    s.parse().ok()
}

pub fn version_header() -> HeaderValue {
    HeaderValue::from_static(concat!("lu/", env!("CARGO_PKG_VERSION")))
}

pub fn with_version(mut res: Response) -> Response {
    res.headers_mut().insert("x-lu-version", version_header());
    res
}

pub fn parse_query(uri: &Uri) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = uri.query() {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            // minimal decoding for '+' and %xx common cases
            let v = percent_decode(v);
            out.insert(k.to_string(), v);
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
