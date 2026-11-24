use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use lu_proto::printable;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Unprocessable,
    Conflict,
    StaleEpoch,
    Precondition,
    IdempotencyConflict,
    TooLarge,
    RateLimited,
    LimitExceeded,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Unprocessable => "unprocessable",
            Self::Conflict => "conflict",
            Self::StaleEpoch => "stale_epoch",
            Self::Precondition => "precondition_failed",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::TooLarge => "too_large",
            Self::RateLimited => "rate_limited",
            Self::LimitExceeded => "limit_exceeded",
            Self::Internal => "internal",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Conflict => StatusCode::CONFLICT,
            Self::StaleEpoch => StatusCode::CONFLICT,
            Self::Precondition => StatusCode::PRECONDITION_FAILED,
            Self::IdempotencyConflict => StatusCode::UNPROCESSABLE_ENTITY,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::LimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub log_length: Option<u64>,
    pub retry_after_secs: Option<u64>,
    /// The configured limit name for a typed `limit_exceeded` response.
    pub limit: Option<String>,
    pub www_authenticate: bool,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            log_length: None,
            retry_after_secs: None,
            limit: None,
            www_authenticate: false,
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, msg)
    }
    pub fn unauthorized() -> Self {
        let mut e = Self::new(ErrorCode::Unauthorized, "unauthorized");
        e.www_authenticate = true;
        e
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, msg)
    }
    pub fn unprocessable(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unprocessable, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, msg)
    }
    pub fn stale_epoch() -> Self {
        Self::new(ErrorCode::StaleEpoch, "stale epoch")
    }
    pub fn precondition(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Precondition, msg)
    }
    pub fn idempotency_conflict() -> Self {
        Self::new(
            ErrorCode::IdempotencyConflict,
            "idempotency key already used for a different request",
        )
    }
    pub fn too_large(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::TooLarge, msg)
    }
    pub fn rate_limited(retry_after_secs: u64) -> Self {
        let mut e = Self::new(ErrorCode::RateLimited, "rate limited");
        e.retry_after_secs = Some(retry_after_secs.max(1));
        e
    }
    pub fn limit_exceeded(limit: &'static str) -> Self {
        let mut e = Self::new(ErrorCode::LimitExceeded, format!("limit exceeded: {limit}"));
        e.limit = Some(limit.to_owned());
        e
    }
    pub fn internal() -> Self {
        Self::new(ErrorCode::Internal, "internal error")
    }

    pub fn with_log_length(mut self, len: u64) -> Self {
        self.log_length = Some(len);
        self
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<String>,
}

/// Returns a message safe to emit: control chars neutralized and bounded.
/// Never echoes raw untrusted input; callers must pass only fixed or sanitized context.
pub fn safe_message(msg: &str) -> String {
    let p = printable(msg);
    // keep reasonably short
    if p.len() > 512 {
        let mut out = p.chars().take(512).collect::<String>();
        out.push('…');
        out
    } else {
        p
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.code.as_str();
        let body = ErrorBody {
            error: code,
            message: safe_message(&self.message),
            length: self.log_length,
            limit: self.limit,
        };
        let json = serde_json::to_string(&body).unwrap_or_else(|_| {
            "{\"error\":\"internal\",\"message\":\"internal error\"}".to_owned()
        });
        let mut res = (
            self.code.status(),
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response();
        if self.www_authenticate {
            res.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(secs) = self.retry_after_secs {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                res.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        res
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
