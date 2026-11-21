//! HTTP(S) listener. The accept loop caps connections globally and per remote address, times out
//! a client that never finishes its headers, watches the whole connection for stalls (including
//! a client that never reads a response), and drains in-flight connections on shutdown.

mod limit;
mod request_id;
mod shutdown;
pub mod tls;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::IntoResponse;
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use limit::AddrLimit;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tower_service::Service;

pub use request_id::{with_request_ids, X_REQUEST_ID};
pub use shutdown::shutdown_signal;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
const WATCHDOG_CHECK: Duration = Duration::from_millis(250);
const WATCHDOG_MIN_CHECK: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeOptions {
    pub max_connections: usize,
    pub max_connections_per_addr: usize,
    pub header_read_timeout: Duration,
    /// Longest a handler may take; a request that overruns it gets a 408.
    pub request_timeout: Duration,
    /// Longest a connection may be quiet between requests, or stalled while writing a response.
    /// An active request is allowed `request_timeout + connection_idle_timeout`.
    pub connection_idle_timeout: Duration,
}

/// The remote socket address of a connection. The listener puts it in every request's
/// extensions, so handlers can key rate limits on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAddr(pub SocketAddr);

/// Serves `router` until `shutdown` resolves. TLS is used when `tls` is set.
///
/// Refused connections are dropped without a response. A connection that is still open when the
/// drain timeout expires is abandoned.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    options: ServeOptions,
    tls: Option<Arc<rustls::ServerConfig>>,
    shutdown: impl Future<Output = ()>,
) {
    let slots = Arc::new(Semaphore::new(options.max_connections));
    let per_addr = AddrLimit::new(options.max_connections_per_addr);
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let Some(addr_slot) = per_addr.acquire(peer.ip()) else {
            tracing::debug!(%peer, "per-address connection limit reached");
            continue;
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tracing::debug!(%peer, "connection limit reached");
            continue;
        };
        let router = router.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _addr_slot = addr_slot;
            if let Err(error) = handle(stream, peer, router, tls, options).await {
                tracing::debug!(%peer, %error, "connection ended with an error");
            }
        });
    }
    drop(listener);
    // Let in-flight requests finish, but not forever.
    let all = u32::try_from(options.max_connections).unwrap_or(u32::MAX);
    if tokio::time::timeout(DRAIN_TIMEOUT, slots.acquire_many(all))
        .await
        .is_err()
    {
        tracing::warn!("shutdown: connections still open after the drain timeout");
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

async fn handle(
    stream: TcpStream,
    peer: SocketAddr,
    router: Router,
    tls: Option<Arc<rustls::ServerConfig>>,
    options: ServeOptions,
) -> Result<(), BoxError> {
    match tls {
        None => connection(stream, router, peer, options).await?,
        Some(config) => {
            let accept = tokio_rustls::TlsAcceptor::from(config).accept(stream);
            let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, accept)
                .await
                .map_err(|_| "TLS handshake timed out")??;
            connection(stream, router, peer, options).await?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Activity {
    last: Instant,
    in_flight: usize,
}

impl Activity {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            in_flight: 0,
        }
    }
}

fn activity_lock(activity: &Mutex<Activity>) -> std::sync::MutexGuard<'_, Activity> {
    activity
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn connection<IO>(
    io: IO,
    router: Router,
    peer: SocketAddr,
    options: ServeOptions,
) -> Result<(), BoxError>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let activity = Arc::new(Mutex::new(Activity::new()));
    let service_activity = Arc::clone(&activity);
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mut router = router.clone();
        let activity = Arc::clone(&service_activity);
        let request_timeout = options.request_timeout;
        let mut req = req.map(Body::new);
        req.extensions_mut().insert(PeerAddr(peer));
        async move {
            {
                let mut active = activity_lock(&activity);
                active.in_flight += 1;
                active.last = Instant::now();
            }
            let response = match tokio::time::timeout(request_timeout, router.call(req)).await {
                Ok(response) => response,
                Err(_) => Ok((
                    axum::http::StatusCode::REQUEST_TIMEOUT,
                    "request timed out\n",
                )
                    .into_response()),
            };
            {
                let mut active = activity_lock(&activity);
                active.in_flight = active.in_flight.saturating_sub(1);
                active.last = Instant::now();
            }
            response
        }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(options.header_read_timeout)
        .serve_connection(TokioIo::new(io), service);
    idle_watchdog(
        connection,
        Arc::clone(&activity),
        options.connection_idle_timeout,
        options.request_timeout,
    )
    .await
}

/// A whole-connection watchdog. The per-request timeout guards handler work; this one also covers
/// a client that stops reading a response or goes quiet on a keep-alive connection. An in-flight
/// request gets `request_timeout` of grace so a 25-second log wait is not mistaken for a stall.
async fn idle_watchdog<F, T>(
    connection: F,
    activity: Arc<Mutex<Activity>>,
    idle_timeout: Duration,
    request_timeout: Duration,
) -> Result<T, BoxError>
where
    F: Future<Output = Result<T, hyper::Error>>,
{
    let mut connection = Box::pin(connection);
    let check_every = idle_timeout.min(WATCHDOG_CHECK).max(WATCHDOG_MIN_CHECK);
    let mut ticker = tokio::time::interval(check_every);
    loop {
        tokio::select! {
            result = &mut connection => return result.map_err(Into::into),
            _ = ticker.tick() => {
                let (last, in_flight) = {
                    let active = activity_lock(&activity);
                    (active.last, active.in_flight)
                };
                let allowance = if in_flight > 0 {
                    request_timeout.saturating_add(idle_timeout)
                } else {
                    idle_timeout
                };
                if last.elapsed() >= allowance {
                    return Err(format!("connection idle for {allowance:?}").into());
                }
            }
        }
    }
}
