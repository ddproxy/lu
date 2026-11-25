//! The node's Reiki health, read from the local Reiki peer endpoint and sent with each
//! heartbeat, so the controller and `lu top` can show which providers a node can use.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use lu_proto::{
    valid_provider_name, NodeFailureReason, ProviderState, ProviderStatus, MAX_PROVIDERS,
};
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// How often the local Reiki is asked. A heartbeat reuses the last answer.
const REFRESH: Duration = Duration::from_secs(10);
/// A reply larger than this is ignored; a health document is a few kilobytes.
const MAX_REPLY_BYTES: usize = 256 * 1024;
/// The controller refuses a `reiki` field over 64 KiB; stay well under it.
const MAX_FORWARDED_BYTES: usize = 48 * 1024;
/// Failed reads in a row before the node reports its health as unknown.
const STALE_AFTER_FAILURES: u32 = 3;

/// What a heartbeat carries about providers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Health {
    pub providers: Vec<ProviderStatus>,
    pub reiki: Option<Value>,
    /// A node fault reported by Reiki itself, never inferred from harness output.
    pub infra_reason: Option<NodeFailureReason>,
    /// Counts successful reads, so a watcher can tell a new reading from a repeated one.
    pub seq: u64,
}

/// The latest answer, shared between the refresher and the heartbeats.
pub type Shared = Arc<Mutex<Health>>;

pub fn snapshot(shared: &Shared) -> Health {
    shared
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Turns a Reiki peer document into a heartbeat's provider list. Anything unreadable is
/// skipped, never guessed: the document comes from a local process but is still parsed
/// defensively.
pub fn parse(doc: &Value) -> Health {
    let providers = doc
        .get("upstreams")
        .and_then(Value::as_array)
        .map(|upstreams| {
            upstreams
                .iter()
                .filter_map(provider)
                .take(MAX_PROVIDERS)
                .collect()
        })
        .unwrap_or_default();
    let forwarded = serde_json::to_vec(doc)
        .map(|bytes| bytes.len() <= MAX_FORWARDED_BYTES)
        .unwrap_or(false);
    Health {
        providers,
        reiki: forwarded.then(|| doc.clone()),
        infra_reason: None,
        seq: 0,
    }
}

fn provider(upstream: &Value) -> Option<ProviderStatus> {
    let name = upstream.get("name")?.as_str()?;
    if !valid_provider_name(name) {
        return None;
    }
    let count = |key: &str| upstream.get(key).and_then(Value::as_u64).unwrap_or(0);
    let (healthy, cooling, disabled) = (
        count("healthy_keys"),
        count("cooling_keys"),
        count("disabled_keys"),
    );
    let state = if healthy > 0 {
        ProviderState::Ok
    } else if cooling > 0 {
        ProviderState::Cooling
    } else if disabled > 0 {
        ProviderState::Disabled
    } else {
        ProviderState::Unknown
    };
    let soonest_secs = (state == ProviderState::Cooling)
        .then(|| upstream.get("soonest_cooldown_seconds")?.as_u64())
        .flatten();
    Some(ProviderStatus {
        name: name.to_owned(),
        state,
        soonest_secs,
    })
}

#[derive(Debug)]
enum Fetch {
    Health(Value),
    GatewayFault,
    Failed,
}

async fn read_body(response: &mut reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_REPLY_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

async fn fetch(client: &reqwest::Client, url: &str, token: &str) -> Fetch {
    let mut response = match client.get(url).bearer_auth(token).send().await {
        Ok(response) => response,
        Err(_) => return Fetch::Failed,
    };
    if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return match read_body(&mut response).await {
            Some(body) if crate::infra::reiki_gateway_503(&String::from_utf8_lossy(&body)) => {
                Fetch::GatewayFault
            }
            _ => Fetch::Failed,
        };
    }
    let mut response = match response.error_for_status() {
        Ok(response) => response,
        Err(_) => return Fetch::Failed,
    };
    match read_body(&mut response).await {
        Some(body) => serde_json::from_slice(&body)
            .ok()
            .map_or(Fetch::Failed, Fetch::Health),
        None => Fetch::Failed,
    }
}

/// Reads the local Reiki every few seconds into `shared` until the task is aborted.
pub fn spawn_refresher(url: String, token: Arc<str>, shared: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                warn!(%error, "cannot build the Reiki health client; provider health stays unknown");
                return;
            }
        };
        let mut failures = 0u32;
        loop {
            match fetch(&client, &url, &token).await {
                Fetch::Health(doc) => {
                    failures = 0;
                    let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    let seq = guard.seq.wrapping_add(1);
                    *guard = Health { seq, ..parse(&doc) };
                }
                Fetch::GatewayFault => {
                    failures = 0;
                    let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    let seq = guard.seq.wrapping_add(1);
                    *guard = Health {
                        seq,
                        infra_reason: Some(NodeFailureReason::ReikiGateway503),
                        ..Health::default()
                    };
                }
                Fetch::Failed => {
                    failures = failures.saturating_add(1);
                    debug!(failures, "could not read the local Reiki health");
                    if failures == STALE_AFTER_FAILURES {
                        let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                        let seq = guard.seq;
                        *guard = Health {
                            seq,
                            ..Health::default()
                        };
                    }
                }
            }
            tokio::time::sleep(REFRESH).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn each_upstream_becomes_a_provider_with_the_right_state() {
        let doc = json!({"instance":"node-a","upstreams":[
            {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0},
            {"name":"openrouter","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":90},
            {"name":"forgejo","healthy_keys":0,"cooling_keys":0,"disabled_keys":1},
            {"name":"empty","healthy_keys":0,"cooling_keys":0,"disabled_keys":0}
        ]});
        let health = parse(&doc);
        let got: Vec<(&str, ProviderState, Option<u64>)> = health
            .providers
            .iter()
            .map(|p| (p.name.as_str(), p.state, p.soonest_secs))
            .collect();
        assert_eq!(
            got,
            vec![
                ("zen", ProviderState::Ok, None),
                ("openrouter", ProviderState::Cooling, Some(90)),
                ("forgejo", ProviderState::Disabled, None),
                ("empty", ProviderState::Unknown, None),
            ]
        );
        assert!(health.reiki.is_some());
    }

    #[test]
    fn a_healthy_key_wins_over_a_cooling_one() {
        let doc = json!({"upstreams":[{"name":"zen","healthy_keys":1,"cooling_keys":2,"soonest_cooldown_seconds":5}]});
        let health = parse(&doc);
        assert_eq!(health.providers[0].state, ProviderState::Ok);
        assert_eq!(health.providers[0].soonest_secs, None);
    }

    #[test]
    fn hostile_documents_give_nothing_worse_than_an_empty_list() {
        for doc in [
            json!(null),
            json!("text"),
            json!({"upstreams":"nope"}),
            json!({"upstreams":[null, 3, {"name":42}, {"name":"has space"}, {"name":""}]}),
            json!({"upstreams":[{"name":"ok","healthy_keys":-5,"cooling_keys":"many"}]}),
        ] {
            let health = parse(&doc);
            assert!(
                health
                    .providers
                    .iter()
                    .all(|p| valid_provider_name(&p.name)),
                "{doc}"
            );
        }
        let unknown = parse(&json!({"upstreams":[{"name":"ok","healthy_keys":-5}]}));
        assert_eq!(unknown.providers[0].state, ProviderState::Unknown);
    }

    #[test]
    fn at_most_the_provider_cap_and_an_oversized_document_is_not_forwarded() {
        let many: Vec<Value> = (0..200)
            .map(|i| json!({"name": format!("p{i}"), "healthy_keys": 1}))
            .collect();
        assert_eq!(
            parse(&json!({ "upstreams": many })).providers.len(),
            MAX_PROVIDERS
        );
        let big = json!({"upstreams": [], "padding": "x".repeat(MAX_FORWARDED_BYTES + 1)});
        assert!(parse(&big).reiki.is_none());
    }

    async fn fetch_with_reply(body: &'static [u8]) -> Fetch {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        let client = reqwest::Client::new();
        let fetched = fetch(&client, &format!("http://{addr}/health"), "test").await;
        server.await.unwrap();
        fetched
    }

    #[tokio::test]
    async fn a_named_reiki_503_is_a_gateway_fault() {
        let fetched = fetch_with_reply(br#"{"error":"gateway_store_unavailable"}"#).await;
        assert!(matches!(fetched, Fetch::GatewayFault), "{fetched:?}");
    }

    #[tokio::test]
    async fn an_unnamed_503_is_not_a_node_fault() {
        let fetched = fetch_with_reply(b"service unavailable").await;
        assert!(matches!(fetched, Fetch::Failed), "{fetched:?}");
    }
}
