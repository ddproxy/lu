//! Per-job Unix socket exposed to the harness.
//!
//! The harness sends small one-line JSON requests. The agent never trusts fields that identify
//! the caller: `job_id` and `epoch` are overwritten here before forwarding, so a harness cannot
//! use its socket to act as another job. Peer credentials are checked on every connection, and
//! the listener is dropped as soon as the job loop ends.

use std::collections::VecDeque;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{chown, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lu_client::{ApiError, Controller};
use lu_proto::{
    decode_socket_line, printable, truncate_chars, JobId, ScopeAction, ScopeErrorResponse,
    ScopeRequest, MAX_SCOPE_REQUEST_BYTES,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Maximum requests per job socket in a rolling minute. The controller has the real spawn cap;
/// this one stops a local harness from flooding the agent with cheap malformed requests.
pub const SOCKET_RATE_LIMIT: usize = 120;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Requests and replies are one line each. This is allowance for JSON escaping around a 64 KiB
/// task plus framing; the decoder still enforces the exact byte cap.
const WIRE_SLACK: usize = 4096;

/// Owns the listener and its private directory. Dropping it stops the accept loop and removes the
/// socket, so every terminal path in the job loop tears the endpoint down.
#[derive(Debug)]
pub struct JobSocket {
    path: PathBuf,
    dir: PathBuf,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Drop for JobSocket {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

impl JobSocket {
    /// Creates a fresh private socket for one job lease. The directory is unique per attempt, so a
    /// retry always gets a new path and a stale harness connection cannot be reused.
    pub fn start(
        controller: Arc<dyn Controller>,
        job_id: &JobId,
        epoch: u64,
        work_dir: &Path,
        owner: Option<(u32, u32)>,
    ) -> Result<Self, String> {
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let scope_root = work_dir.join("scope");
        std::fs::create_dir_all(&scope_root)
            .map_err(|error| format!("cannot create job socket root: {error}"))?;
        set_mode(&scope_root, 0o700)?;
        if let Some((uid, gid)) = owner {
            set_owner(&scope_root, uid, gid)?;
        }
        let dir = scope_root.join(format!("{job_id}-{epoch}-{nanos}-{nonce}"));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create job socket dir: {error}"))?;
        set_mode(&dir, 0o700)?;
        if let Some((uid, gid)) = owner {
            set_owner(&dir, uid, gid)?;
        }
        let path = dir.join("job.sock");
        let listener = UnixListener::bind(&path)
            .map_err(|error| format!("cannot bind job socket: {error}"))?;
        set_mode(&path, 0o600)?;
        if let Some((uid, gid)) = owner {
            set_owner(&path, uid, gid)?;
        }
        // With an explicitly configured harness account, peer credentials are checked against
        // that uid. Otherwise the harness is expected to share the agent's uid, which is the uid
        // the socket was created with.
        let uid = owner.map_or_else(
            || {
                std::fs::metadata(&path)
                    .map(|meta| meta.uid())
                    .map_err(|error| format!("cannot stat job socket: {error}"))
            },
            |(uid, _)| Ok(uid),
        )?;
        let (stop, stop_rx) = watch::channel(false);
        let state = Arc::new(SocketState {
            controller,
            job_id: job_id.clone(),
            epoch,
            uid,
            rate: Mutex::new(VecDeque::new()),
        });
        let task = tokio::spawn(accept_loop(listener, state, stop_rx));
        Ok(Self {
            path,
            dir,
            stop,
            task,
        })
    }

    /// The path passed to the harness as `LU_JOB_SOCKET`. Never contains a token or controller URL.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("cannot set permissions on {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_owner(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    chown(path, Some(uid), Some(gid))
        .map_err(|error| format!("cannot chown {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_owner(_path: &Path, _uid: u32, _gid: u32) -> Result<(), String> {
    Ok(())
}

struct SocketState {
    controller: Arc<dyn Controller>,
    job_id: JobId,
    epoch: u64,
    uid: u32,
    rate: Mutex<VecDeque<Instant>>,
}

impl SocketState {
    fn allow_request(&self) -> bool {
        let Ok(mut seen) = self.rate.lock() else {
            return false;
        };
        let cutoff = Instant::now()
            .checked_sub(RATE_WINDOW)
            .unwrap_or(Instant::now());
        while seen.front().is_some_and(|at| *at < cutoff) {
            seen.pop_front();
        }
        if seen.len() >= SOCKET_RATE_LIMIT {
            return false;
        }
        seen.push_back(Instant::now());
        true
    }
}

async fn accept_loop(
    listener: UnixListener,
    state: Arc<SocketState>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            handle_connection(stream, state).await;
                        });
                    }
                    Err(error) => {
                        // A transient accept error must not take down the whole job.
                        tracing::debug!(%error, "job socket accept failed");
                    }
                }
            }
        }
    }
}

/// Safe peer-credential check. Kept as a pure function so it can be tested without changing uids.
pub(crate) fn peer_allowed(peer_uid: u32, expected_uid: u32) -> bool {
    peer_uid == expected_uid
}

async fn handle_connection(mut stream: UnixStream, state: Arc<SocketState>) {
    let cred = match stream.peer_cred() {
        Ok(cred) => cred,
        Err(_) => return,
    };
    if !peer_allowed(cred.uid(), state.uid) {
        let _ = write_scope_error(&mut stream, ApiError::Forbidden, None, None).await;
        return;
    }
    let bytes = match read_bounded_line(&mut stream).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let error = ApiError::BadRequest {
                status: 400,
                message: "invalid socket request".to_owned(),
            };
            let _ = write_scope_error(&mut stream, error, None, None).await;
            return;
        }
    };
    if !state.allow_request() {
        let error = ApiError::LimitExceeded {
            limit: "scope_request_rate".to_owned(),
        };
        let _ = write_scope_error(&mut stream, error, None, None).await;
        return;
    }
    let mut request = match decode_socket_line(&bytes) {
        Ok(request) => request,
        Err(_) => {
            let error = ApiError::BadRequest {
                status: 400,
                message: "invalid scope request".to_owned(),
            };
            let _ = write_scope_error(&mut stream, error, None, None).await;
            return;
        }
    };
    // The harness never supplies its identity. Both fields are replaced here, so a request cannot
    // be replayed against a different job or lease through this socket.
    request.job_id = state.job_id.clone();
    request.epoch = state.epoch;
    let reply = match state.controller.scope(&request).await {
        Ok(response) => serde_json::to_vec(&response).unwrap_or_else(|_| {
            serde_json::to_vec(&ScopeErrorResponse::new("internal", "internal error"))
                .unwrap_or_default()
        }),
        Err(error) => {
            let (code, limit) = api_code(&error);
            serde_json::to_vec(&error_body(error, Some(&code), limit)).unwrap_or_default()
        }
    };
    let _ = stream.write_all(&reply).await;
    let _ = stream.write_all(b"\n").await;
    let _ = stream.flush().await;
}

async fn read_bounded_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if line.len().saturating_add(read) > MAX_SCOPE_REQUEST_BYTES + WIRE_SLACK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scope request line too long",
            ));
        }
        line.extend_from_slice(&chunk[..read]);
        if line.contains(&b'\n') {
            break;
        }
    }
    Ok(line)
}

async fn write_scope_error(
    stream: &mut UnixStream,
    error: ApiError,
    code: Option<&str>,
    limit: Option<String>,
) -> io::Result<()> {
    let body = error_body(error, code, limit);
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    write_line(stream, &bytes).await
}

async fn write_line(stream: &mut UnixStream, body: &[u8]) -> io::Result<()> {
    stream.write_all(body).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await
}

fn error_body(error: ApiError, code: Option<&str>, limit: Option<String>) -> ScopeErrorResponse {
    let code = code
        .map(str::to_owned)
        .unwrap_or_else(|| api_code(&error).0.to_owned());
    let message = truncate_chars(&printable(&error.message()), 512);
    let mut body = ScopeErrorResponse::new(code, message);
    if let Some(limit) = limit.or_else(|| api_limit(&error)) {
        body = body.with_limit(limit);
    }
    body
}

fn api_code(error: &ApiError) -> (String, Option<String>) {
    let code = match error {
        ApiError::StaleEpoch => "stale_epoch",
        ApiError::Conflict { .. } => "conflict",
        ApiError::Unauthorized => "unauthorized",
        ApiError::Forbidden => "forbidden",
        ApiError::NotFound => "not_found",
        ApiError::TooLarge { .. } => "too_large",
        ApiError::Redirect { .. } => "bad_request",
        ApiError::BadRequest { .. } => "bad_request",
        ApiError::TooManyRequests { .. } => "rate_limited",
        ApiError::LimitExceeded { .. } => "limit_exceeded",
        ApiError::Server { .. } => "internal",
        ApiError::Network { .. } | ApiError::Decode { .. } | ApiError::Config { .. } => "internal",
    };
    (code.to_owned(), api_limit(error))
}

fn api_limit(error: &ApiError) -> Option<String> {
    match error {
        ApiError::LimitExceeded { limit } => Some(limit.clone()),
        _ => None,
    }
}

/// Builds a socket request for callers such as the scoped MCP server. Identity fields are
/// placeholders; [`JobSocket`] replaces them before forwarding.
pub fn socket_request(
    action: ScopeAction,
    payload: Option<lu_proto::ScopePayload>,
) -> ScopeRequest {
    let mut request = ScopeRequest::new(JobId::new(), 1, action);
    if let Some(payload) = payload {
        request = request.with_payload(payload);
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Call, FakeController};
    use lu_proto::{ScopeResponse, ScopeResult, SCOPE_SOURCE};
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn peer_check_accepts_only_the_expected_uid() {
        assert!(peer_allowed(1000, 1000));
        assert!(!peer_allowed(1001, 1000));
    }

    #[test]
    fn error_body_is_tagged_and_never_empty() {
        let body = error_body(
            ApiError::LimitExceeded {
                limit: "max_depth".to_owned(),
            },
            None,
            None,
        );
        assert_eq!(body.source, SCOPE_SOURCE);
        assert!(body.untrusted);
        assert_eq!(body.error.code, "limit_exceeded");
        assert_eq!(body.error.limit.as_deref(), Some("max_depth"));
    }

    #[test]
    fn socket_request_uses_a_placeholder_identity() {
        let request = socket_request(ScopeAction::JobContext, None);
        assert!(request.epoch > 0);
        assert_eq!(request.action, ScopeAction::JobContext);
    }

    async fn read_line(stream: &mut UnixStream) -> String {
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).trim().to_owned()
    }

    #[tokio::test]
    async fn socket_is_private_and_forwards_the_real_job_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let controller = Arc::new(FakeController::new());
        let job_id = JobId::new();
        let socket = JobSocket::start(controller.clone(), &job_id, 7, tmp.path(), None).unwrap();

        let dir_mode = std::fs::metadata(socket.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let socket_mode = std::fs::metadata(socket.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(socket_mode, 0o600);
        assert!(socket
            .path()
            .to_string_lossy()
            .contains(&job_id.to_string()));

        controller.push_scope(Ok(ScopeResponse::new(ScopeResult::SubmitChild {
            job_id: JobId::new(),
        })));
        let mut stream = UnixStream::connect(socket.path()).await.unwrap();
        let mut forged = socket_request(
            ScopeAction::SubmitChild,
            Some(lu_proto::ScopePayload {
                task: Some("child task".to_owned()),
                ..lu_proto::ScopePayload::default()
            }),
        );
        forged.job_id = JobId::new();
        forged.epoch = 999;
        let mut line = serde_json::to_vec(&forged).unwrap();
        line.push(b'\n');
        stream.write_all(&line).await.unwrap();
        stream.flush().await.unwrap();
        let reply = read_line(&mut stream).await;
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["source"], SCOPE_SOURCE);
        assert_eq!(value["untrusted"], true);
        assert_eq!(value["result"]["kind"], "submit_child");

        controller.check(|calls| {
            let Some(Call::Scope(request)) = calls.first() else {
                panic!("no scope call recorded: {calls:?}");
            };
            assert_eq!(request.job_id, job_id);
            assert_eq!(request.epoch, 7);
            assert_eq!(request.action, ScopeAction::SubmitChild);
        });
    }

    #[tokio::test]
    async fn socket_can_be_owned_by_the_configured_harness_account() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = std::fs::metadata(tmp.path()).unwrap();
        let owner = (meta.uid(), meta.gid());
        let socket = JobSocket::start(
            Arc::new(FakeController::new()),
            &JobId::new(),
            3,
            tmp.path(),
            Some(owner),
        )
        .unwrap();

        let dir_meta = std::fs::metadata(socket.path().parent().unwrap()).unwrap();
        let socket_meta = std::fs::metadata(socket.path()).unwrap();
        assert_eq!(dir_meta.uid(), owner.0);
        assert_eq!(dir_meta.gid(), owner.1);
        assert_eq!(socket_meta.uid(), owner.0);
        assert_eq!(socket_meta.gid(), owner.1);
        let mut stream = UnixStream::connect(socket.path()).await.unwrap();
        let mut line = serde_json::to_vec(&socket_request(ScopeAction::JobContext, None)).unwrap();
        line.push(b'\n');
        stream.write_all(&line).await.unwrap();
        stream.flush().await.unwrap();
        let reply = read_line(&mut stream).await;
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["source"], SCOPE_SOURCE);
    }

    #[tokio::test]
    async fn socket_disappears_when_the_job_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let controller = Arc::new(FakeController::new());
        let socket = JobSocket::start(controller, &JobId::new(), 1, tmp.path(), None).unwrap();
        let path = socket.path().to_owned();
        let dir = path.parent().unwrap().to_owned();
        assert!(path.exists());
        drop(socket);
        tokio::task::yield_now().await;
        assert!(!path.exists());
        assert!(!dir.exists());
        assert!(UnixStream::connect(&path).await.is_err());
    }

    #[tokio::test]
    async fn a_retry_gets_a_fresh_socket_path() {
        let tmp = tempfile::tempdir().unwrap();
        let job = JobId::new();
        let first =
            JobSocket::start(Arc::new(FakeController::new()), &job, 1, tmp.path(), None).unwrap();
        let first_path = first.path().to_owned();
        let second =
            JobSocket::start(Arc::new(FakeController::new()), &job, 2, tmp.path(), None).unwrap();
        let second_path = second.path().to_owned();
        assert_ne!(first_path, second_path);
        assert!(first_path.exists());
        assert!(second_path.exists());
        drop(first);
        tokio::task::yield_now().await;
        assert!(!first_path.exists());
        assert!(second_path.exists());
        let mut stream = UnixStream::connect(&second_path).await.unwrap();
        assert!(stream.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn peer_with_a_different_uid_is_refused() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let expected = std::fs::metadata("/proc/self")
            .map(|meta| meta.uid())
            .unwrap_or(0)
            + 1;
        let state = Arc::new(SocketState {
            controller: Arc::new(FakeController::new()),
            job_id: JobId::new(),
            epoch: 1,
            uid: expected,
            rate: Mutex::new(VecDeque::new()),
        });
        let task = tokio::spawn(handle_connection(server, state));
        client
            .write_all(b"{\"job_id\":\"x\",\"epoch\":1,\"action\":\"job_context\"}\n")
            .await
            .unwrap();
        client.flush().await.unwrap();
        let reply = read_line(&mut client).await;
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["source"], SCOPE_SOURCE);
        assert_eq!(value["untrusted"], true);
        assert_eq!(value["error"]["code"], "forbidden");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_socket_lines_are_rejected_before_decoding() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let mut oversized = vec![b'x'; MAX_SCOPE_REQUEST_BYTES + WIRE_SLACK + 1];
        oversized.push(b'\n');
        let writer = tokio::spawn(async move {
            client.write_all(&oversized).await.unwrap();
            client.flush().await.unwrap();
        });
        assert!(read_bounded_line(&mut server).await.is_err());
        writer.await.unwrap();
    }

    #[test]
    fn socket_rate_limit_is_bounded() {
        let state = SocketState {
            controller: Arc::new(FakeController::new()),
            job_id: JobId::new(),
            epoch: 1,
            uid: 0,
            rate: Mutex::new(VecDeque::new()),
        };
        for _ in 0..SOCKET_RATE_LIMIT {
            assert!(state.allow_request());
        }
        assert!(!state.allow_request());
    }
}
