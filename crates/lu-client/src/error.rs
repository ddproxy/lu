//! API failures. The variants the protocol asks callers to act on (a stale lease, an
//! idempotency conflict, a rejected token, a transport failure) are distinct; everything else is
//! one of the general status buckets.

use std::fmt;

use lu_proto::{printable, truncate_chars};

/// Why a controller call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The request carried a fencing epoch older than the controller's lease.
    StaleEpoch,
    /// An idempotency key was reused with a different payload.
    Conflict { length: u64 },
    /// The bearer token was missing, wrong, or not allowed to use the route.
    Unauthorized,
    /// The token was valid but lacks the scope for this route.
    Forbidden,
    /// The job, node, or artifact does not exist.
    NotFound,
    /// The requested resource is larger than the client will read.
    TooLarge { limit: u64 },
    /// A 3xx response. The client never follows redirects (a forged Location could leak the
    /// token to another host).
    Redirect { status: u16 },
    /// A 4xx response not covered by a more specific variant.
    BadRequest { status: u16, message: String },
    /// The controller asked the client to slow down.
    TooManyRequests { retry_after: Option<u64> },
    /// A named scope limit was reached. The name is safe to show and to use as a metric label.
    LimitExceeded { limit: String },
    /// A 5xx response.
    Server { status: u16, message: String },
    /// The request never produced a complete HTTP response.
    Network { message: String },
    /// The response was not the JSON the route promises.
    Decode { message: String },
    /// The base URL, CA bundle, or token file was refused before any request was sent.
    Config { message: String },
}

impl ApiError {
    /// The HTTP status, when the error came from a response.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Redirect { status } | Self::BadRequest { status, .. } => Some(*status),
            Self::Server { status, .. } => Some(*status),
            Self::Unauthorized => Some(401),
            Self::Forbidden => Some(403),
            Self::NotFound => Some(404),
            Self::Conflict { .. } | Self::StaleEpoch => Some(409),
            Self::TooManyRequests { .. } | Self::LimitExceeded { .. } => Some(429),
            Self::TooLarge { .. }
            | Self::Network { .. }
            | Self::Decode { .. }
            | Self::Config { .. } => None,
        }
    }

    /// Whether trying the same request again could succeed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Network { .. } | Self::Server { .. } | Self::TooManyRequests { .. }
        )
    }

    /// Removes a secret that a hostile controller may have echoed back into an error body.
    pub(crate) fn redact(&mut self, secret: &str) {
        if secret.is_empty() {
            return;
        }
        let message = match self {
            Self::BadRequest { message, .. }
            | Self::Server { message, .. }
            | Self::Network { message }
            | Self::Decode { message }
            | Self::Config { message } => message,
            _ => return,
        };
        if message.contains(secret) {
            *message = message.replace(secret, "<redacted>");
        }
    }

    /// Fuzz-only entry point for the redaction that `Client::scrub` applies to every error.
    #[cfg(feature = "fuzzing")]
    pub fn redact_for_fuzzing(&mut self, secret: &str) {
        self.redact(secret);
    }

    /// A short, display-safe explanation. Never includes the bearer token.
    pub fn message(&self) -> String {
        match self {
            Self::StaleEpoch => "the lease epoch is stale".to_owned(),
            Self::Conflict { length } => {
                format!("idempotency key conflict (payload length {length})")
            }
            Self::Unauthorized => "unauthorized".to_owned(),
            Self::Forbidden => "forbidden".to_owned(),
            Self::NotFound => "not found".to_owned(),
            Self::TooLarge { limit } => format!("response is larger than {limit} bytes"),
            Self::Redirect { status } => format!("refused a redirect (HTTP {status})"),
            Self::BadRequest { status, message } => format!("HTTP {status}: {message}"),
            Self::TooManyRequests { retry_after } => match retry_after {
                Some(seconds) => format!("rate limited; retry after {seconds}s"),
                None => "rate limited".to_owned(),
            },
            Self::LimitExceeded { limit } => format!("limit exceeded: {limit}"),
            Self::Server { status, message } => format!("HTTP {status}: {message}"),
            Self::Network { message } => message.clone(),
            Self::Decode { message } => message.clone(),
            Self::Config { message } => message.clone(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ApiError {}

/// Keeps a body excerpt small and harmless before it reaches a terminal or an error string.
pub(crate) fn body_excerpt(body: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(body);
    let one_line = text.replace(['\r', '\n'], " ");
    printable(&truncate_chars(one_line.trim(), max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_body_text_is_neutralised_in_messages() {
        let message = body_excerpt(b"bad\x1b[31m\nnext", 64);
        assert_eq!(message, "bad?[31m next");
    }

    #[test]
    fn a_conflict_keeps_its_length() {
        let error = ApiError::Conflict { length: 42 };
        assert_eq!(error.status(), Some(409));
        assert!(error.to_string().contains("42"));
    }
}
