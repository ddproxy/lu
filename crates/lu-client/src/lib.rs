//! A typed async client for the Lu controller API.
//!
//! The client is used by agents (poll, lease renewal, logs, artifacts, completion) and by the
//! operator CLI. It authenticates every request with the bearer token, refuses redirects, caps
//! every response body, and retries only idempotent GETs. The token is redacted from `Debug` and
//! from error text.
//!
//! The controller contract this crate implements lives in the repository's API document; the
//! route names are also kept in one place here so a fake controller in a test can mirror them.

mod client;
mod error;
mod types;

pub use client::{Client, ClientOptions, Controller};
pub use error::ApiError;
pub use types::{
    CompletionBody, HostStats, JobStats, JobView, ListJobs, ListNodes, ListRepos, LogPage,
    NodeStats, NodeView, PollReply, PollRequest, ProcessStats, RepoView, StatusView,
    MAX_JSON_BYTES,
};

/// Canonical paths, exactly as documented in docs/api.md; the client sends each of these and
/// nothing else.
pub mod routes {
    pub const JOBS: &str = "/v1/jobs";
    pub const JOB: &str = "/v1/jobs/{id}";
    pub const LOGS: &str = "/v1/jobs/{id}/logs";
    pub const ARTIFACT: &str = "/v1/jobs/{id}/artifacts/{name}";
    pub const CANCEL: &str = "/v1/jobs/{id}/cancel";
    pub const STATUS: &str = "/v1/status";
    pub const REPOS: &str = "/v1/repos";
    pub const REPO_SYNC: &str = "/v1/repos/{name}/sync";
    pub const NODES: &str = "/v1/nodes";
    pub const NODE_STATS: &str = "/v1/nodes/{node}/stats";
    pub const DRAIN: &str = "/v1/nodes/{node}/{action}";
    pub const POLL: &str = "/v1/agent/poll";
    pub const START: &str = "/v1/agent/jobs/{id}/start";
    pub const RENEW: &str = "/v1/agent/jobs/{id}/renew";
    pub const APPEND_LOG: &str = "/v1/agent/jobs/{id}/log";
    pub const PUT_ARTIFACT: &str = "/v1/agent/jobs/{id}/artifacts/{name}";
    pub const COMPLETE: &str = "/v1/agent/jobs/{id}/complete";
}
