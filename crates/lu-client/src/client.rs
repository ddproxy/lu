//! The HTTP client. Every request is authenticated, bounded, and never follows a redirect.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use lu_proto::{
    AgentEvent, ArtifactName, Completion, JobId, JobSpec, JobState, LogChunk, NodeName,
};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::error::{body_excerpt, ApiError};
use crate::types::{
    CompletionBody, JobEnvelope, JobView, ListJobs, ListNodes, LogPage, NodeView, PollReply,
    PollRequest, MAX_JSON_BYTES,
};

type Result<T> = std::result::Result<T, ApiError>;

const TOKEN_LIMIT: u64 = 8 * 1024;
const CA_LIMIT: u64 = 1024 * 1024;

/// Timeouts and caps. The defaults are deliberately finite; a controller URL is not trusted to
/// answer promptly or with a small body.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Extra PEM certificates to trust, in addition to the system roots.
    pub ca_file: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_json_bytes: u64,
    pub max_attempts: u32,
    pub backoff: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            ca_file: None,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            max_json_bytes: MAX_JSON_BYTES,
            max_attempts: 3,
            backoff: Duration::from_millis(100),
        }
    }
}

/// A bearer token that cannot leak through `Debug` or an error message.
#[derive(Clone)]
struct SecretToken(String);

impl SecretToken {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The parts of a request that every retry reuses. Grouped so the transport signature stays
/// readable as the number of headers grows.
struct SendParts<'a> {
    body: Option<Vec<u8>>,
    query: &'a [(String, String)],
    headers: &'a [(String, String)],
}

/// An authenticated controller client. Cheap to clone; the underlying connection pool is shared.
/// A controller base URL that `lu_config::validate_controller_url` accepted (scheme, host and port rules,
/// plain http only on loopback), with no query, fragment or dot segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpUrl(String);

impl HttpUrl {
    pub(crate) fn parse(url: &str) -> std::result::Result<Self, String> {
        lu_config::validate_controller_url(url).map_err(|e| e.to_string())?;
        if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err("url must not contain whitespace or control characters".to_owned());
        }
        if url.contains('?') || url.contains('#') {
            return Err("url must not have a query or fragment".to_owned());
        }
        let after_scheme = url.split_once("://").map_or("", |(_, rest)| rest);
        let path = after_scheme.find('/').map_or("", |i| &after_scheme[i..]);
        if path.split('/').any(|seg| seg == ".." || seg == ".") {
            return Err("url path must not contain . or .. segments".to_owned());
        }
        Ok(Self(url.to_owned()))
    }

    pub(crate) fn as_string(&self) -> String {
        self.0.clone()
    }
}

#[derive(Clone)]
pub struct Client {
    base: HttpUrl,
    http: reqwest::Client,
    token: SecretToken,
    max_json_bytes: u64,
    max_attempts: u32,
    backoff: Duration,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_string())
            .field("token", &"<redacted>")
            .field("max_json_bytes", &self.max_json_bytes)
            .field("max_attempts", &self.max_attempts)
            .finish()
    }
}

impl Client {
    /// Builds a client. `ca_file` is an optional extra PEM trust anchor. Plain http is accepted
    /// only for a loopback host, which is what [`lu_config::parse_http_url`] enforces.
    pub fn new(base_url: &str, token: &str, ca_file: Option<PathBuf>) -> Result<Self> {
        Self::new_with_options(
            base_url,
            token,
            ClientOptions {
                ca_file,
                ..ClientOptions::default()
            },
        )
    }

    pub fn new_with_options(base_url: &str, token: &str, options: ClientOptions) -> Result<Self> {
        let base = HttpUrl::parse(base_url).map_err(|message| ApiError::Config { message })?;
        if token.len() as u64 > TOKEN_LIMIT {
            return Err(ApiError::Config {
                message: format!("token is larger than {TOKEN_LIMIT} bytes"),
            });
        }
        if token.chars().any(|c| c.is_control()) {
            return Err(ApiError::Config {
                message: "token contains a control character".to_owned(),
            });
        }
        let mut builder = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("lu-client/", env!("CARGO_PKG_VERSION")));
        if let Some(path) = &options.ca_file {
            let cert = read_ca(path)?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder.build().map_err(|error| ApiError::Config {
            message: format!("could not build the HTTP client: {}", safe_reqwest(error)),
        })?;
        Ok(Self {
            base,
            http,
            token: SecretToken(token.to_owned()),
            max_json_bytes: options.max_json_bytes,
            max_attempts: options.max_attempts.max(1),
            backoff: options.backoff,
        })
    }

    /// The base URL with the token removed, for display.
    pub fn base_url(&self) -> String {
        self.base.as_string()
    }

    // ---- operator ---------------------------------------------------------

    pub async fn submit(&self, spec: &JobSpec) -> Result<JobView> {
        let reply = self.submit_reply(spec).await?;
        self.scrub(parse_job(&reply.body))
    }

    /// The raw submit response, so the CLI can implement `--json` without re-encoding.
    pub async fn submit_raw(&self, spec: &JobSpec) -> Result<String> {
        let reply = self.submit_reply(spec).await?;
        Ok(raw_body(&reply))
    }

    /// Both the parsed view and the exact response body, from one request.
    pub async fn submit_with_raw(&self, spec: &JobSpec) -> Result<(JobView, String)> {
        let reply = self.submit_reply(spec).await?;
        let view = self.scrub(parse_job(&reply.body))?;
        Ok((view, raw_body(&reply)))
    }

    async fn submit_reply(&self, spec: &JobSpec) -> Result<HttpReply> {
        let body = serde_json::to_vec(spec).map_err(|error| ApiError::Decode {
            message: format!("could not encode the job spec: {error}"),
        })?;
        let mut headers = Vec::new();
        if let Some(key) = &spec.idempotency_key {
            headers.push(("Idempotency-Key".to_owned(), key.clone()));
        }
        let reply = self
            .send(
                Method::POST,
                &owned(&["v1/jobs", "v1/job", "jobs"]),
                Some(body),
                &[],
                &headers,
                false,
            )
            .await?;
        self.success(reply)
    }

    pub async fn list_jobs(&self, state: Option<JobState>, limit: Option<u32>) -> Result<ListJobs> {
        let mut query = Vec::new();
        if let Some(state) = state {
            let name = serde_json::to_value(state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "queued".to_owned());
            query.push(("state".to_owned(), name));
        }
        if let Some(limit) = limit {
            query.push(("limit".to_owned(), limit.to_string()));
        }
        let reply = self
            .send(
                Method::GET,
                &owned(&["v1/jobs", "jobs"]),
                None,
                &query,
                &[],
                true,
            )
            .await?;
        let reply = self.success(reply)?;
        self.scrub(parse_job_list(&reply.body))
    }

    /// The raw list response, so the operator CLI can print what the controller sent.
    pub async fn list_jobs_raw(
        &self,
        state: Option<JobState>,
        limit: Option<u32>,
    ) -> Result<String> {
        let mut query = Vec::new();
        if let Some(state) = state {
            let name = serde_json::to_value(state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "queued".to_owned());
            query.push(("state".to_owned(), name));
        }
        if let Some(limit) = limit {
            query.push(("limit".to_owned(), limit.to_string()));
        }
        let reply = self
            .send(
                Method::GET,
                &owned(&["v1/jobs", "jobs"]),
                None,
                &query,
                &[],
                true,
            )
            .await?;
        let reply = self.success(reply)?;
        Ok(raw_body(&reply))
    }

    pub async fn get_job(&self, id: &JobId) -> Result<JobView> {
        let routes = [format!("v1/jobs/{id}"), format!("jobs/{id}")];
        let reply = self
            .send(Method::GET, &routes, None, &[], &[], true)
            .await?;
        let reply = self.success(reply)?;
        self.scrub(parse_job(&reply.body))
    }

    /// The raw job response, so the operator CLI can print what the controller sent.
    pub async fn get_job_raw(&self, id: &JobId) -> Result<String> {
        let routes = [format!("v1/jobs/{id}"), format!("jobs/{id}")];
        let reply = self
            .send(Method::GET, &routes, None, &[], &[], true)
            .await?;
        let reply = self.success(reply)?;
        Ok(raw_body(&reply))
    }

    /// Alias for [`Client::get_job`].
    pub async fn show_job(&self, id: &JobId) -> Result<JobView> {
        self.get_job(id).await
    }

    pub async fn logs(&self, id: &JobId, offset: u64, wait: Option<u64>) -> Result<LogPage> {
        let reply = self.logs_reply(id, offset, wait).await?;
        Ok(parse_log(&reply, offset))
    }

    /// Both the parsed page and the exact response body, from one request.
    pub async fn logs_with_raw(
        &self,
        id: &JobId,
        offset: u64,
        wait: Option<u64>,
    ) -> Result<(LogPage, String)> {
        let reply = self.logs_reply(id, offset, wait).await?;
        let page = parse_log(&reply, offset);
        let raw = raw_body(&reply);
        Ok((page, raw))
    }

    async fn logs_reply(&self, id: &JobId, offset: u64, wait: Option<u64>) -> Result<HttpReply> {
        let mut query = vec![("offset".to_owned(), offset.to_string())];
        if let Some(wait) = wait {
            query.push(("wait".to_owned(), wait.to_string()));
        }
        let routes = [
            format!("v1/jobs/{id}/logs"),
            format!("v1/jobs/{id}/log"),
            format!("jobs/{id}/logs"),
        ];
        let reply = self
            .send(Method::GET, &routes, None, &query, &[], true)
            .await?;
        self.success(reply)
    }

    /// The raw log response. Follow mode prints one raw page per line.
    pub async fn logs_raw(&self, id: &JobId, offset: u64, wait: Option<u64>) -> Result<String> {
        let reply = self.logs_reply(id, offset, wait).await?;
        Ok(raw_body(&reply))
    }

    pub async fn fetch_artifact(&self, id: &JobId, name: ArtifactName) -> Result<Vec<u8>> {
        let file = name.file_name();
        let routes = [
            format!("v1/jobs/{id}/artifacts/{file}"),
            format!("v1/jobs/{id}/{file}"),
            format!("jobs/{id}/{file}"),
        ];
        let reply = self
            .send_capped(
                Method::GET,
                &routes,
                SendParts {
                    body: None,
                    query: &[],
                    headers: &[],
                },
                true,
                name.max_bytes(),
            )
            .await?;
        let reply = self.success(reply)?;
        let cap = name.max_bytes();
        if reply.body.len() as u64 > cap {
            return Err(ApiError::TooLarge { limit: cap });
        }
        Ok(reply.body)
    }

    pub async fn cancel(&self, id: &JobId) -> Result<JobView> {
        let body = serde_json::to_vec(&serde_json::json!({ "job_id": id })).map_err(|error| {
            ApiError::Decode {
                message: format!("could not encode the cancel request: {error}"),
            }
        })?;
        let routes = [format!("v1/jobs/{id}/cancel"), format!("jobs/{id}/cancel")];
        let reply = self
            .send(Method::POST, &routes, Some(body), &[], &[], false)
            .await?;
        if matches!(
            reply.status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            // Some controllers expose cancellation as DELETE on the job itself.
            let routes = [format!("v1/jobs/{id}"), format!("jobs/{id}")];
            let reply = self
                .send(Method::DELETE, &routes, None, &[], &[], false)
                .await?;
            return self.cancel_reply(reply, id);
        }
        self.cancel_reply(reply, id)
    }

    fn cancel_reply(&self, reply: HttpReply, id: &JobId) -> Result<JobView> {
        let reply = self.success(reply)?;
        if reply.body.is_empty() {
            return Ok(JobView::placeholder(id.clone(), JobState::Cancelled));
        }
        self.scrub(parse_job(&reply.body))
    }

    /// The raw cancel response. An empty body means the controller cancelled without a view.
    pub async fn cancel_raw(&self, id: &JobId) -> Result<String> {
        let body = serde_json::to_vec(&serde_json::json!({ "job_id": id })).map_err(|error| {
            ApiError::Decode {
                message: format!("could not encode the cancel request: {error}"),
            }
        })?;
        let routes = [format!("v1/jobs/{id}/cancel"), format!("jobs/{id}/cancel")];
        let reply = self
            .send(Method::POST, &routes, Some(body), &[], &[], false)
            .await?;
        if matches!(
            reply.status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            let routes = [format!("v1/jobs/{id}"), format!("jobs/{id}")];
            let reply = self
                .send(Method::DELETE, &routes, None, &[], &[], false)
                .await?;
            let reply = self.success(reply)?;
            return Ok(raw_body(&reply));
        }
        let reply = self.success(reply)?;
        Ok(raw_body(&reply))
    }

    pub async fn nodes(&self) -> Result<ListNodes> {
        let reply = self
            .send(
                Method::GET,
                &owned(&["v1/nodes", "nodes"]),
                None,
                &[],
                &[],
                true,
            )
            .await?;
        let reply = self.success(reply)?;
        self.scrub(parse_nodes(&reply.body))
    }

    /// Alias for [`Client::nodes`].
    pub async fn list_nodes(&self) -> Result<ListNodes> {
        self.nodes().await
    }

    /// The raw node-list response.
    pub async fn nodes_raw(&self) -> Result<String> {
        let reply = self
            .send(
                Method::GET,
                &owned(&["v1/nodes", "nodes"]),
                None,
                &[],
                &[],
                true,
            )
            .await?;
        let reply = self.success(reply)?;
        Ok(raw_body(&reply))
    }

    pub async fn set_drain(&self, node: &NodeName, drain: bool) -> Result<NodeView> {
        let action = if drain { "drain" } else { "undrain" };
        let routes = [
            format!("v1/nodes/{node}/{action}"),
            format!("v1/nodes/{node}/cordon"),
            format!("nodes/{node}/{action}"),
        ];
        let body =
            serde_json::to_vec(&serde_json::json!({ "cordoned": drain })).map_err(|error| {
                ApiError::Decode {
                    message: format!("could not encode the node request: {error}"),
                }
            })?;
        let reply = self
            .send(Method::POST, &routes, Some(body), &[], &[], false)
            .await?;
        let reply = self.success(reply)?;
        if reply.body.is_empty() {
            return Ok(NodeView::placeholder(node.clone(), drain));
        }
        self.scrub(parse_node(&reply.body))
    }

    pub async fn drain(&self, node: &NodeName) -> Result<NodeView> {
        self.set_drain(node, true).await
    }

    /// The raw drain/undrain response.
    pub async fn set_drain_raw(&self, node: &NodeName, drain: bool) -> Result<String> {
        let action = if drain { "drain" } else { "undrain" };
        let routes = [
            format!("v1/nodes/{node}/{action}"),
            format!("v1/nodes/{node}/cordon"),
            format!("nodes/{node}/{action}"),
        ];
        let body =
            serde_json::to_vec(&serde_json::json!({ "cordoned": drain })).map_err(|error| {
                ApiError::Decode {
                    message: format!("could not encode the node request: {error}"),
                }
            })?;
        let reply = self
            .send(Method::POST, &routes, Some(body), &[], &[], false)
            .await?;
        let reply = self.success(reply)?;
        Ok(raw_body(&reply))
    }

    pub async fn undrain(&self, node: &NodeName) -> Result<NodeView> {
        self.set_drain(node, false).await
    }

    // ---- agent ------------------------------------------------------------

    pub async fn poll(&self, request: &PollRequest) -> Result<Option<AgentEvent>> {
        let body = serde_json::to_vec(request).map_err(|error| ApiError::Decode {
            message: format!("could not encode the poll request: {error}"),
        })?;
        let reply = self
            .send(
                Method::POST,
                &owned(&["v1/agent/poll", "v1/poll", "agent/poll"]),
                Some(body),
                &[],
                &[],
                false,
            )
            .await?;
        if reply.status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let reply = self.success(reply)?;
        if reply.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        // A controller may wrap the event; accept both forms.
        let event = serde_json::from_slice::<PollReply>(&reply.body)
            .map(PollReply::into_event)
            .or_else(|_| serde_json::from_slice::<AgentEvent>(&reply.body))
            .map_err(|error| ApiError::Decode {
                message: format!("could not decode the poll reply: {error}"),
            });
        Ok(Some(self.scrub(event)?))
    }

    pub async fn start(&self, id: &JobId, epoch: u64) -> Result<()> {
        self.lease_ack("start", id, epoch).await
    }

    pub async fn renew(&self, id: &JobId, epoch: u64) -> Result<()> {
        self.lease_ack("renew", id, epoch).await
    }

    async fn lease_ack(&self, action: &str, id: &JobId, epoch: u64) -> Result<()> {
        let body = serde_json::to_vec(&serde_json::json!({ "job_id": id, "epoch": epoch }))
            .map_err(|error| ApiError::Decode {
                message: format!("could not encode the {action} request: {error}"),
            })?;
        let routes = [
            format!("v1/agent/jobs/{id}/{action}"),
            format!("v1/jobs/{id}/{action}"),
            format!("v1/agent/{action}/{id}"),
        ];
        let headers = [("X-Lu-Epoch".to_owned(), epoch.to_string())];
        let reply = self
            .send(Method::POST, &routes, Some(body), &[], &headers, false)
            .await?;
        self.success(reply).map(|_| ())
    }

    pub async fn append_log(&self, chunk: &LogChunk) -> Result<()> {
        chunk.validate().map_err(|_| ApiError::TooLarge {
            limit: lu_proto::MAX_LOG_CHUNK_BYTES as u64,
        })?;
        let body = serde_json::to_vec(chunk).map_err(|error| ApiError::Decode {
            message: format!("could not encode the log chunk: {error}"),
        })?;
        let id = &chunk.job_id;
        let routes = [
            format!("v1/agent/jobs/{id}/logs"),
            format!("v1/agent/jobs/{id}/log"),
            format!("v1/jobs/{id}/logs"),
        ];
        let headers = [("X-Lu-Epoch".to_owned(), chunk.epoch.to_string())];
        let reply = self
            .send(Method::POST, &routes, Some(body), &[], &headers, false)
            .await?;
        self.success(reply).map(|_| ())
    }

    pub async fn put_artifact(
        &self,
        id: &JobId,
        epoch: u64,
        name: ArtifactName,
        bytes: &[u8],
    ) -> Result<()> {
        if bytes.len() as u64 > name.max_bytes() {
            return Err(ApiError::TooLarge {
                limit: name.max_bytes(),
            });
        }
        let file = name.file_name();
        let routes = [
            format!("v1/agent/jobs/{id}/artifacts/{file}"),
            format!("v1/jobs/{id}/artifacts/{file}"),
        ];
        let query = [("epoch".to_owned(), epoch.to_string())];
        let headers = [
            ("X-Lu-Epoch".to_owned(), epoch.to_string()),
            (
                "Content-Type".to_owned(),
                "application/octet-stream".to_owned(),
            ),
        ];
        let reply = self
            .send(
                Method::PUT,
                &routes,
                Some(bytes.to_vec()),
                &query,
                &headers,
                false,
            )
            .await?;
        self.success(reply).map(|_| ())
    }

    pub async fn complete(&self, completion: &Completion) -> Result<()> {
        completion
            .validate()
            .map_err(|error| ApiError::BadRequest {
                status: 0,
                message: error.to_string(),
            })?;
        let body = serde_json::to_vec(&CompletionBody {
            job_id: completion.job_id.clone(),
            epoch: completion.epoch,
            outcome: completion.outcome,
            exit_code: completion.exit_code,
            summary: completion.summary.clone(),
        })
        .map_err(|error| ApiError::Decode {
            message: format!("could not encode the completion: {error}"),
        })?;
        let id = &completion.job_id;
        let routes = [
            format!("v1/agent/jobs/{id}/complete"),
            format!("v1/jobs/{id}/complete"),
            "v1/agent/complete".to_owned(),
        ];
        let headers = [("X-Lu-Epoch".to_owned(), completion.epoch.to_string())];
        let reply = self
            .send(Method::POST, &routes, Some(body), &[], &headers, false)
            .await?;
        self.success(reply).map(|_| ())
    }

    // ---- transport --------------------------------------------------------

    async fn send(
        &self,
        method: Method,
        routes: &[String],
        body: Option<Vec<u8>>,
        query: &[(String, String)],
        headers: &[(String, String)],
        retry: bool,
    ) -> Result<HttpReply> {
        self.send_capped(
            method,
            routes,
            SendParts {
                body,
                query,
                headers,
            },
            retry,
            self.max_json_bytes,
        )
        .await
    }

    async fn send_capped(
        &self,
        method: Method,
        routes: &[String],
        parts: SendParts<'_>,
        retry: bool,
        max_body: u64,
    ) -> Result<HttpReply> {
        let mut last_not_found = None;
        for (index, route) in routes.iter().enumerate() {
            let url = self.build_url(route, parts.query);
            let mut attempt = 0u32;
            loop {
                match self
                    .send_once(&method, &url, parts.body.clone(), parts.headers, max_body)
                    .await
                {
                    Ok(reply) => {
                        let transient = retry
                            && (reply.status == StatusCode::TOO_MANY_REQUESTS
                                || reply.status.is_server_error());
                        if transient && attempt + 1 < self.max_attempts {
                            attempt += 1;
                            tokio::time::sleep(self.backoff_for(attempt)).await;
                            continue;
                        }
                        if index + 1 < routes.len()
                            && matches!(
                                reply.status,
                                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                            )
                        {
                            last_not_found = Some(reply);
                            break;
                        }
                        return Ok(reply);
                    }
                    Err(error)
                        if retry && error.is_transient() && attempt + 1 < self.max_attempts =>
                    {
                        attempt += 1;
                        tokio::time::sleep(self.backoff_for(attempt)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        match last_not_found {
            Some(reply) => Ok(reply),
            None => Err(ApiError::NotFound),
        }
    }

    async fn send_once(
        &self,
        method: &Method,
        url: &str,
        body: Option<Vec<u8>>,
        headers: &[(String, String)],
        max_body: u64,
    ) -> Result<HttpReply> {
        let mut request = self
            .http
            .request(method.clone(), url)
            .bearer_auth(self.token.expose());
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.map_err(network_error)?;
        let status = response.status();
        let headers = response.headers().clone();
        if let Some(length) = response.content_length() {
            if length > max_body && !status.is_success() {
                return Ok(HttpReply {
                    status,
                    headers,
                    body: Vec::new(),
                });
            }
            if length > max_body {
                return Err(ApiError::TooLarge { limit: max_body });
            }
        }
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            let new_len = body.len().saturating_add(chunk.len());
            if new_len as u64 > max_body && status.is_success() {
                return Err(ApiError::TooLarge { limit: max_body });
            }
            // Error bodies are only read for diagnostics; keep those bounded too.
            if new_len as u64 > max_body {
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpReply {
            status,
            headers,
            body,
        })
    }

    fn build_url(&self, route: &str, query: &[(String, String)]) -> String {
        let base = self.base.as_string();
        let base = base.trim_end_matches('/');
        let route = route.trim_start_matches('/');
        let mut url = format!("{base}/{route}");
        if !query.is_empty() {
            url.push('?');
            for (index, (key, value)) in query.iter().enumerate() {
                if index > 0 {
                    url.push('&');
                }
                url.push_str(&percent_encode(key));
                url.push('=');
                url.push_str(&percent_encode(value));
            }
        }
        url
    }

    fn backoff_for(&self, attempt: u32) -> Duration {
        let factor = 1u32 << attempt.min(5);
        self.backoff.saturating_mul(factor)
    }

    /// Removes the bearer token from errors built while parsing a success body, too: a hostile
    /// controller can echo it from a 200 response just as easily as from a 4xx one.
    fn scrub<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|mut error| {
            error.redact(self.token.expose());
            error
        })
    }

    fn success(&self, reply: HttpReply) -> Result<HttpReply> {
        if reply.status.is_success() {
            return Ok(reply);
        }
        let mut error = error_from_reply(&reply);
        error.redact(self.token.expose());
        Err(error)
    }
}

/// The six calls an agent needs. Implemented by [`Client`] and by test fakes.
pub trait Controller: Send + Sync {
    fn poll<'a>(
        &'a self,
        request: &'a PollRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentEvent>>> + Send + 'a>>;
    fn start<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    fn renew<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    fn append_log<'a>(
        &'a self,
        chunk: &'a LogChunk,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    fn put_artifact<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
        name: ArtifactName,
        bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    fn complete<'a>(
        &'a self,
        completion: &'a Completion,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

impl Controller for Client {
    fn poll<'a>(
        &'a self,
        request: &'a PollRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentEvent>>> + Send + 'a>> {
        Box::pin(async move { Client::poll(self, request).await })
    }

    fn start<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { Client::start(self, id, epoch).await })
    }

    fn renew<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { Client::renew(self, id, epoch).await })
    }

    fn append_log<'a>(
        &'a self,
        chunk: &'a LogChunk,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { Client::append_log(self, chunk).await })
    }

    fn put_artifact<'a>(
        &'a self,
        id: &'a JobId,
        epoch: u64,
        name: ArtifactName,
        bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { Client::put_artifact(self, id, epoch, name, bytes).await })
    }

    fn complete<'a>(
        &'a self,
        completion: &'a Completion,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { Client::complete(self, completion).await })
    }
}

struct HttpReply {
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

fn read_ca(path: &Path) -> Result<reqwest::Certificate> {
    let bytes = std::fs::read(path).map_err(|error| ApiError::Config {
        message: format!("cannot read CA file {}: {error}", path.display()),
    })?;
    if bytes.len() as u64 > CA_LIMIT {
        return Err(ApiError::Config {
            message: format!("CA file {} is larger than {CA_LIMIT} bytes", path.display()),
        });
    }
    reqwest::Certificate::from_pem(&bytes).map_err(|error| ApiError::Config {
        message: format!(
            "CA file {} is not a PEM certificate: {error}",
            path.display()
        ),
    })
}

fn network_error(error: reqwest::Error) -> ApiError {
    let message = if error.is_timeout() {
        "the request timed out".to_owned()
    } else if error.is_connect() {
        "could not connect to the controller".to_owned()
    } else {
        format!("the request failed: {}", safe_reqwest(error))
    };
    ApiError::Network { message }
}

fn safe_reqwest(error: reqwest::Error) -> String {
    body_excerpt(error.without_url().to_string().as_bytes(), 512)
}

fn error_from_reply(reply: &HttpReply) -> ApiError {
    let text = body_excerpt(&reply.body, 512);
    let value = serde_json::from_slice::<Value>(&reply.body).ok();
    let code = value.as_ref().and_then(error_code);
    match (reply.status, code.as_deref()) {
        (_, Some("stale_epoch")) | (StatusCode::GONE, _) => ApiError::StaleEpoch,
        (_, Some("conflict")) | (StatusCode::CONFLICT, _) => ApiError::Conflict {
            length: value.as_ref().and_then(conflict_length).unwrap_or(0),
        },
        (_, Some("unauthorized")) | (StatusCode::UNAUTHORIZED, _) => ApiError::Unauthorized,
        (_, Some("forbidden")) | (StatusCode::FORBIDDEN, _) => ApiError::Forbidden,
        (_, Some("not_found")) | (StatusCode::NOT_FOUND, _) => ApiError::NotFound,
        (_, Some("too_large")) | (StatusCode::PAYLOAD_TOO_LARGE, _) => ApiError::TooLarge {
            limit: value.as_ref().and_then(conflict_length).unwrap_or(0),
        },
        (StatusCode::MOVED_PERMANENTLY, _)
        | (StatusCode::FOUND, _)
        | (StatusCode::SEE_OTHER, _)
        | (StatusCode::TEMPORARY_REDIRECT, _)
        | (StatusCode::PERMANENT_REDIRECT, _) => ApiError::Redirect {
            status: reply.status.as_u16(),
        },
        (StatusCode::TOO_MANY_REQUESTS, _) => ApiError::TooManyRequests {
            retry_after: retry_after(reply),
        },
        (status, _) if status.is_server_error() => ApiError::Server {
            status: status.as_u16(),
            message: text,
        },
        (status, _) => ApiError::BadRequest {
            status: status.as_u16(),
            message: text,
        },
    }
}

fn error_code(value: &Value) -> Option<String> {
    let direct = value
        .get("error")
        .or_else(|| value.get("code"))
        .or_else(|| value.get("reason"));
    if let Some(Value::String(code)) = direct {
        return Some(code.to_ascii_lowercase());
    }
    if let Some(object) = value.get("error").and_then(Value::as_object) {
        return object
            .get("code")
            .or_else(|| object.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
    }
    None
}

fn conflict_length(value: &Value) -> Option<u64> {
    for key in ["length", "payload_length", "bytes"] {
        if let Some(number) = value.get(key).and_then(Value::as_u64) {
            return Some(number);
        }
        if let Some(nested) = value.get("error").and_then(|error| error.get(key)) {
            if let Some(number) = nested.as_u64() {
                return Some(number);
            }
        }
    }
    None
}

fn retry_after(reply: &HttpReply) -> Option<u64> {
    reply
        .headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn parse_job(body: &[u8]) -> Result<JobView> {
    if let Ok(view) = serde_json::from_slice::<JobView>(body) {
        return Ok(view);
    }
    if let Ok(envelope) = serde_json::from_slice::<JobEnvelope>(body) {
        return Ok(envelope.job);
    }
    if let Ok(id) = serde_json::from_slice::<JobId>(body) {
        return Ok(JobView::placeholder(id, JobState::Queued));
    }
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(job) = value.get("job") {
            if let Ok(view) = serde_json::from_value::<JobView>(job.clone()) {
                return Ok(view);
            }
        }
        // A submit route may answer with only the new id, optionally with its state.
        let id = ["job_id", "id"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .and_then(|text| JobId::parse(text).ok());
        if let Some(id) = id {
            let state = value
                .get("state")
                .and_then(|state| serde_json::from_value::<JobState>(state.clone()).ok())
                .unwrap_or(JobState::Queued);
            return Ok(JobView::placeholder(id, state));
        }
    }
    Err(ApiError::Decode {
        message: format!("could not decode a job: {}", body_excerpt(body, 200)),
    })
}

fn parse_job_list(body: &[u8]) -> Result<ListJobs> {
    if let Ok(list) = serde_json::from_slice::<ListJobs>(body) {
        return Ok(list);
    }
    if let Ok(jobs) = serde_json::from_slice::<Vec<JobView>>(body) {
        return Ok(ListJobs {
            jobs,
            total: None,
            next: None,
        });
    }
    // A controller may use `items` for the bit that matters and add other fields.
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(items) = value
            .get("items")
            .or_else(|| value.get("jobs"))
            .and_then(Value::as_array)
        {
            let mut jobs = Vec::with_capacity(items.len());
            for item in items {
                let view = serde_json::from_value::<JobView>(item.clone()).map_err(|error| {
                    ApiError::Decode {
                        message: format!("could not decode a job: {error}"),
                    }
                })?;
                jobs.push(view);
            }
            return Ok(ListJobs {
                jobs,
                total: value.get("total").and_then(Value::as_u64),
                next: value.get("next").and_then(Value::as_str).map(str::to_owned),
            });
        }
    }
    Err(ApiError::Decode {
        message: format!("could not decode the job list: {}", body_excerpt(body, 200)),
    })
}

fn parse_nodes(body: &[u8]) -> Result<ListNodes> {
    if let Ok(list) = serde_json::from_slice::<ListNodes>(body) {
        return Ok(list);
    }
    if let Ok(nodes) = serde_json::from_slice::<Vec<NodeView>>(body) {
        return Ok(ListNodes { nodes });
    }
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(items) = value
            .get("items")
            .or_else(|| value.get("nodes"))
            .and_then(Value::as_array)
        {
            let mut nodes = Vec::with_capacity(items.len());
            for item in items {
                nodes.push(
                    serde_json::from_value::<NodeView>(item.clone()).map_err(|error| {
                        ApiError::Decode {
                            message: format!("could not decode a node: {error}"),
                        }
                    })?,
                );
            }
            return Ok(ListNodes { nodes });
        }
    }
    Err(ApiError::Decode {
        message: format!(
            "could not decode the node list: {}",
            body_excerpt(body, 200)
        ),
    })
}

fn parse_node(body: &[u8]) -> Result<NodeView> {
    if let Ok(node) = serde_json::from_slice::<NodeView>(body) {
        return Ok(node);
    }
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(node) = value.get("node") {
            return serde_json::from_value::<NodeView>(node.clone()).map_err(|error| {
                ApiError::Decode {
                    message: format!("could not decode a node: {error}"),
                }
            });
        }
    }
    Err(ApiError::Decode {
        message: format!("could not decode a node: {}", body_excerpt(body, 200)),
    })
}

fn parse_log(reply: &HttpReply, requested_offset: u64) -> LogPage {
    let json = serde_json::from_slice::<LogPage>(&reply.body).ok();
    if let Some(mut page) = json {
        if page.offset == 0 && !page.data.is_empty() {
            page.offset = requested_offset.saturating_add(page.data.len() as u64);
        }
        return page;
    }
    let data = String::from_utf8_lossy(&reply.body).into_owned();
    let offset = reply
        .headers
        .get("x-lu-offset")
        .or_else(|| reply.headers.get("x-next-offset"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| requested_offset.saturating_add(data.len() as u64));
    LogPage {
        data,
        offset,
        state: None,
        terminal: false,
    }
}

fn owned(routes: &[&str]) -> Vec<String> {
    routes.iter().map(|route| (*route).to_owned()).collect()
}

fn raw_body(reply: &HttpReply) -> String {
    String::from_utf8_lossy(&reply.body).into_owned()
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
