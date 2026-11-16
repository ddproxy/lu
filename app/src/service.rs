//! The demo HTTP service: a health route and one bearer-authenticated route. Replace the routes
//! with your own; the listener, auth and request ids stay.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use lu_auth::{bearer_token, LoginThrottle, ThrottleSettings, TokenSet};
use lu_config::Config;
use lu_serve::{serve, shutdown_signal, with_request_ids, PeerAddr, ServeOptions};
use serde_json::json;
use tokio::net::TcpListener;

pub struct AppState {
    tokens: TokenSet,
    /// `None` when throttling is disabled.
    throttle: Option<LoginThrottle>,
}

impl AppState {
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let mut pairs = Vec::new();
        if let Some(controller) = &config.controller {
            for t in &controller.agents {
                pairs.push((t.name.clone(), t.token_sha256.clone()));
            }
            for t in &controller.operators {
                pairs.push((t.name.clone(), t.token_sha256.clone()));
            }
        }
        let tokens = TokenSet::new(pairs).map_err(|e| e.to_string())?;
        let throttle = Some(LoginThrottle::new(ThrottleSettings {
            max_failures: 10,
            window: Duration::from_secs(60),
        }));
        Ok(Self { tokens, throttle })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/whoami", get(whoami))
        .with_state(state)
        // Extractors read at most this much; a raw `Body` has no limit of its own.
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024));
    with_request_ids(app)
}

async fn healthz() -> Response {
    json_response(StatusCode::OK, &json!({ "status": "ok" }))
}

/// JSON without axum's `json` feature, which Reiki does not enable.
fn json_response(status: StatusCode, value: &serde_json::Value) -> Response {
    let body = value.to_string();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn whoami(
    State(state): State<Arc<AppState>>,
    Extension(PeerAddr(peer)): Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = peer.ip();
    let now = Instant::now();
    if let Some(throttle) = &state.throttle {
        if let Some(wait) = throttle.blocked(ip, now) {
            let retry = wait.as_secs().max(1).to_string();
            tracing::warn!(%ip, "login throttled");
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry)],
                "too many failed attempts\n",
            )
                .into_response();
        }
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token);
    match presented.and_then(|token| state.tokens.verify(token)) {
        Some(name) => json_response(StatusCode::OK, &json!({ "name": name })),
        None => {
            if let Some(throttle) = &state.throttle {
                throttle.fail(ip, now);
            }
            tracing::warn!(%ip, "bearer token rejected");
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized\n",
            )
                .into_response()
        }
    }
}

/// Binds the listener and serves until a shutdown signal.
pub async fn run(config: &Config, config_path: &Path) -> Result<(), String> {
    let controller = config
        .controller
        .as_ref()
        .ok_or_else(|| "serve requires a [controller] config".to_owned())?;
    let state = Arc::new(AppState::from_config(config)?);
    let tls = match &config.tls {
        Some(tls) => Some(lu_serve::tls::load(
            Path::new(&tls.cert),
            Path::new(&tls.key),
        )?),
        None => None,
    };
    let listener = TcpListener::bind(&controller.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", controller.listen))?;
    let addr: SocketAddr = listener.local_addr().map_err(|e| e.to_string())?;
    tracing::info!(
        %addr,
        tls = tls.is_some(),
        config = %config_path.display(),
        "listening"
    );
    let options = ServeOptions {
        max_connections: controller.max_connections,
        max_connections_per_addr: controller.max_connections_per_addr,
        header_read_timeout: Duration::from_secs(30),
        request_timeout: Duration::from_secs(60),
    };
    serve(listener, router(state), options, tls, shutdown_signal()).await;
    tracing::info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lu_config::{Config, ControllerAgent, ControllerConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    const TOKEN: &str = "a-long-demo-token-for-tests";

    async fn start(max_failures: u32) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let controller = ControllerConfig {
            listen: "127.0.0.1:7600".to_owned(),
            db: "/db".to_owned(),
            artifacts_dir: "/a".to_owned(),
            lease_secs: 60,
            max_retries: 1,
            provider_cooldown_secs: 1800,
            recovery_grace_secs: 30,
            max_connections: 256,
            max_connections_per_addr: 64,
            agents: vec![ControllerAgent {
                name: "peer-a".to_owned(),
                token_sha256: lu_auth::sha256_hex(TOKEN),
                cordoned: false,
            }],
            operators: Vec::new(),
        };
        let _config = Config {
            controller: Some(controller),
            ..Config::default()
        };
        // override throttle for test
        let state = Arc::new(AppState {
            tokens: TokenSet::new(vec![("peer-a".to_owned(), lu_auth::sha256_hex(TOKEN))]).unwrap(),
            throttle: Some(LoginThrottle::new(ThrottleSettings {
                max_failures,
                window: Duration::from_secs(60),
            })),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let options = ServeOptions {
            max_connections: 16,
            max_connections_per_addr: 8,
            header_read_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
        };
        tokio::spawn(serve(listener, router(state), options, None, async move {
            let _ = stopped.await;
        }));
        (addr, stop)
    }

    async fn send(addr: SocketAddr, headers: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request =
            format!("GET /v1/whoami HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{headers}\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    fn status(response: &str) -> u16 {
        response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn a_correct_bearer_token_gets_the_token_name() {
        let (addr, stop) = start(10).await;
        let reply = send(addr, &format!("Authorization: Bearer {TOKEN}\r\n")).await;
        assert_eq!(status(&reply), 200, "{reply}");
        assert!(reply.ends_with(r#"{"name":"peer-a"}"#), "{reply}");
        let _ = stop.send(());
    }

    #[tokio::test]
    async fn a_missing_or_wrong_token_is_401_with_a_challenge() {
        let (addr, stop) = start(10).await;
        let none = send(addr, "").await;
        assert_eq!(status(&none), 401);
        assert!(
            none.to_ascii_lowercase()
                .contains("www-authenticate: bearer"),
            "{none}"
        );
        assert_eq!(
            status(&send(addr, "Authorization: Bearer nope-nope-nope\r\n").await),
            401
        );
        let _ = stop.send(());
    }

    #[tokio::test]
    async fn hostile_authorization_values_are_401_and_never_crash_the_listener() {
        let (addr, stop) = start(100).await;
        let hostile = [
            "Authorization: Bearer\r\n",
            "Authorization: Bearer a b c\r\n",
            "Authorization: Basic cGVlci1hOng=\r\n",
            "Authorization: Bearer \u{1b}[31m\r\n",
            "Authorization: Bearer t\u{e9}st\r\n",
        ];
        for header in hostile {
            let reply = send(addr, header).await;
            assert!(matches!(status(&reply), 400 | 401), "{header:?} -> {reply}");
        }
        assert_eq!(
            status(&send(addr, &format!("Authorization: Bearer {TOKEN}\r\n")).await),
            200
        );
        let _ = stop.send(());
    }

    #[tokio::test]
    async fn repeated_failures_block_the_address_even_with_the_right_token() {
        let (addr, stop) = start(3).await;
        for _ in 0..3 {
            assert_eq!(
                status(&send(addr, "Authorization: Bearer wrong-wrong-wrong\r\n").await),
                401
            );
        }
        let blocked = send(addr, &format!("Authorization: Bearer {TOKEN}\r\n")).await;
        assert_eq!(status(&blocked), 429, "{blocked}");
        assert!(
            blocked.to_ascii_lowercase().contains("retry-after:"),
            "{blocked}"
        );
        let _ = stop.send(());
    }

    #[test]
    fn a_hash_of_the_wrong_length_is_refused_at_startup() {
        let controller = ControllerConfig {
            listen: "127.0.0.1:7600".to_owned(),
            db: "/db".to_owned(),
            artifacts_dir: "/a".to_owned(),
            lease_secs: 60,
            max_retries: 1,
            provider_cooldown_secs: 1800,
            recovery_grace_secs: 30,
            max_connections: 256,
            max_connections_per_addr: 64,
            agents: vec![ControllerAgent {
                name: "bad".to_owned(),
                token_sha256: "abc".to_owned(),
                cordoned: false,
            }],
            operators: Vec::new(),
        };
        let config = Config {
            controller: Some(controller),
            ..Config::default()
        };
        assert!(AppState::from_config(&config).is_err());
    }
}
