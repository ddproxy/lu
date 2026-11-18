//! stdio MCP server: JSON-RPC core plus read and write tools over the operator API.

mod format;
mod protocol;
mod tools;

use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{io, io::BufRead, pin::Pin};

use lu_client::Client;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};

use protocol::{ToolError, ToolHost, ToolOutput};

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub(crate) struct Host {
    client: Option<Client>,
    error: Option<String>,
    limiter: Mutex<tools::RateLimiter>,
    clock: Clock,
}

impl Host {
    fn base(client: Option<Client>, error: Option<String>) -> Self {
        Self {
            client,
            error,
            limiter: Mutex::new(tools::RateLimiter::new(tools::SUBMIT_LIMIT)),
            clock: Arc::new(format::unix_now),
        }
    }

    fn connected(client: Client) -> Self {
        Self::base(Some(client), None)
    }

    fn failed(error: impl Into<String>) -> Self {
        Self::base(None, Some(error.into()))
    }

    #[cfg(test)]
    fn connected_at(client: Client, clock: Clock) -> Self {
        Self {
            clock,
            ..Self::connected(client)
        }
    }
}

impl ToolHost for Host {
    fn list(&self) -> serde_json::Value {
        tools::list()
    }

    async fn call(&self, name: &str, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        tools::call(self, name, args).await
    }
}

pub(crate) async fn run(config: &Path) -> ExitCode {
    let host = match crate::operator::connect(config) {
        Ok(client) => Host::connected(client),
        Err(error) => {
            // A missing controller must not hide the tools: each call reports the problem.
            eprintln!("lu: {error}");
            Host::failed(error)
        }
    };
    // tokio's `io-std` feature is not enabled, so bridge stdio through blocking threads.
    let (input, _) = stdin_stream();
    let (mut output, output_thread) = stdout_sink();
    let result = serve(input, &mut output, &host).await;
    drop(output);
    let _ = output_thread.join();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lu: mcp server stopped: {error}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn serve<R, W>(reader: R, mut writer: W, host: &Host) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut server = protocol::Server::new();
    while let Some(line) = lines.next_line().await? {
        let response = match protocol::parse(&line) {
            Ok(incoming) => server.handle(incoming, host).await,
            Err(malformed) => Some(protocol::invalid_request(malformed)),
        };
        if let Some(response) = response {
            // serde_json escapes embedded newlines, so one reply stays on one line.
            let text =
                serde_json::to_string(&response).expect("a JSON-RPC reply is always serialisable");
            writer.write_all(text.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
    }
    Ok(())
}

/// Lines from a blocking stdin reader, delivered as an `AsyncRead` stream.
struct StdinStream {
    rx: tokio::sync::mpsc::Receiver<String>,
    pending: Vec<u8>,
}

impl AsyncRead for StdinStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.pending.is_empty() {
                let count = buf.remaining().min(this.pending.len());
                buf.put_slice(&this.pending[..count]);
                this.pending.drain(..count);
                return Poll::Ready(Ok(()));
            }
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(line)) => {
                    let mut bytes = line.into_bytes();
                    bytes.push(b'\n');
                    this.pending = bytes;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn stdin_stream() -> (StdinStream, std::thread::JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(4);
    let handle = std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(std::io::stdin()).lines();
        while let Some(Ok(line)) = lines.next() {
            if tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    (
        StdinStream {
            rx,
            pending: Vec::new(),
        },
        handle,
    )
}

/// Each flushed line goes to a blocking stdout writer.
struct StdoutSink(tokio::sync::mpsc::UnboundedSender<Vec<u8>>);

impl AsyncWrite for StdoutSink {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut().0.send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout writer stopped",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn stdout_sink() -> (StdoutSink, std::thread::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let handle = std::thread::spawn(move || {
        use std::io::Write as _;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        while let Some(bytes) = rx.blocking_recv() {
            if out.write_all(&bytes).is_err() || out.flush().is_err() {
                break;
            }
        }
    });
    (StdoutSink(tx), handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{job_id, job_json, start, Reply, TestServer, TOKEN};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    async fn exchange(script: &str, host: &Host) -> Vec<Value> {
        let reader = BufReader::new(script.as_bytes());
        let mut out = Vec::new();
        serve(reader, &mut out, host).await.unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn request(method: &str, params: Value, id: Value) -> String {
        json!({"jsonrpc": "2.0", "method": method, "params": params, "id": id}).to_string()
    }

    fn initialize(version: &str, id: Value) -> String {
        request("initialize", json!({"protocolVersion": version}), id)
    }

    fn call(name: &str, args: Value, id: Value) -> String {
        request("tools/call", json!({"name": name, "arguments": args}), id)
    }

    fn script(lines: &[String]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    async fn fixture() -> (TestServer, String) {
        let id = job_id();
        let other = job_id();
        let mut hostile = job_json(&id, "running");
        hostile["spec"]["task"] = json!("# Fix the widget\nsecond line of the task");
        hostile["summary"] = json!("final ?[31mred\u{7} done");
        hostile["artifacts"] = json!(["run.log", "diff.patch"]);
        hostile["exit_code"] = json!(0);
        let mut listed = job_json(&other, "queued");
        listed["spec"]["task"] = json!("# Other job");
        listed["summary"] = json!("?[32mgreen done");
        let log = format!("?[31mstart\u{7}\n{}tail\n", "filler ".repeat(14_000));
        let nodes = json!({"nodes": [{
            "name": "node-a", "version": "1.2.3", "harness": "busy",
            "providers": [{"name": "opencode", "state": "cooling", "soonest_secs": 7}],
            "last_seen": 12, "draining": false, "cordons": ["copilot"],
            "load1": 0.5, "mem_available_kb": 1000, "mem_total_kb": 2000, "stuck": 1
        }, {
            "name": "node-old", "version": "1.2.3", "harness": "idle",
            "providers": [], "last_seen": 0, "draining": false, "cordons": [],
            "load1": null, "mem_available_kb": null, "mem_total_kb": null, "stuck": 0
        }]});
        let job_reply = hostile.clone();
        let job_path = id.clone();
        let log_reply = log.clone();
        let list_reply = json!({"jobs": [hostile, listed], "next": null});
        let status_reply = json!({
            "version": "1.2.3", "uptime_secs": 9, "schema_version": 4,
            "db_bytes": 1234, "recovered_on_boot": 0, "jobs": {"queued": 2, "running": 1},
            "oldest_queued_secs": 3, "nodes": 2, "nodes_online": 1, "lease_secs": 60
        });
        let server = start(move |record| match record.path.as_str() {
            "/v1/status" => Reply::json(&status_reply),
            "/v1/jobs" => Reply::json(&list_reply),
            "/v1/nodes" => Reply::json(&nodes),
            path if path.ends_with("/logs") => Reply::json(&json!({
                "data": log_reply, "offset": log_reply.len(), "terminal": true
            })),
            path if path.ends_with(job_path.as_str()) => Reply::json(&job_reply),
            _ => Reply::status(404),
        })
        .await;
        (server, id)
    }

    fn tool_result(reply: &Value) -> (String, Value, bool) {
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        let structured = reply["result"]["structuredContent"].clone();
        assert!(
            !text.contains(TOKEN),
            "tool result leaked the token: {text}"
        );
        assert!(
            !structured.to_string().contains(TOKEN),
            "tool result leaked the token"
        );
        (
            text,
            structured,
            reply["result"]["isError"].as_bool().unwrap(),
        )
    }

    async fn call_tool(host: &Host, name: &str, args: Value) -> Value {
        let lines = script(&initialize_then(&[call(name, args, json!(1))]));
        exchange(&lines, host).await.remove(1)
    }

    fn submitted_job(spec: &Value, state: &str) -> Value {
        let mut job = job_json(&job_id(), state);
        job["spec"] = spec.clone();
        job
    }

    fn post_bodies(server: &TestServer) -> Vec<Value> {
        server
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.method == "POST" && record.path == "/v1/jobs")
            .map(|record| serde_json::from_slice(&record.body).unwrap())
            .collect()
    }

    fn post_keys(server: &TestServer) -> Vec<String> {
        server
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.method == "POST" && record.path == "/v1/jobs")
            .map(|record| {
                record
                    .header("Idempotency-Key")
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    fn posts(server: &TestServer) -> usize {
        server
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.method == "POST")
            .count()
    }

    fn batch_job(name: &str, after: Value) -> Value {
        json!({
            "name": name,
            "repo": "/srv/repo",
            "ref": "main",
            "task": format!("task {name}"),
            "after_names": after
        })
    }

    #[tokio::test]
    async fn handshake_negotiates_versions_and_replies_to_requests() {
        let host = Host::failed("unused");
        let lines = vec![
            initialize("2024-11-05", json!(1)),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned(),
            request("ping", json!({}), json!(2)),
            request("tools/list", json!({}), json!(3)),
            initialize("nonsense", json!(4)),
        ];
        let replies = exchange(&script(&lines), &host).await;
        assert_eq!(replies.len(), 4);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(replies[0]["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "lu");
        assert!(replies[0]["result"]["serverInfo"]["version"].is_string());
        assert!(replies[0]["result"]["instructions"].is_string());
        assert_eq!(replies[1]["result"], json!({}));
        assert_eq!(replies[2]["result"]["tools"].as_array().unwrap().len(), 13);
        assert_eq!(replies[3]["result"]["protocolVersion"], "2025-06-18");
        for (reply, id) in replies.iter().zip([json!(1), json!(2), json!(3), json!(4)]) {
            assert_eq!(reply["id"], id);
            assert_eq!(reply["jsonrpc"], "2.0");
        }
    }

    #[tokio::test]
    async fn string_and_numeric_ids_are_echoed_and_notifications_are_silent() {
        let host = Host::failed("unused");
        let lines = vec![
            initialize("2025-03-26", json!("init-string")),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned(),
            request("ping", json!({}), json!(-42)),
            r#"{"jsonrpc":"2.0","method":"ping","id":null}"#.to_owned(),
            request("ping", Value::Null, json!("p-2")),
        ];
        let replies = exchange(&script(&lines), &host).await;
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0]["id"], "init-string");
        assert_eq!(replies[1]["id"], -42);
        assert_eq!(replies[2]["id"], "p-2");
    }

    #[tokio::test]
    async fn protocol_errors_have_the_right_codes() {
        let host = Host::failed("unused");
        let lines = vec![
            initialize("2025-06-18", json!(1)),
            request("no/such/method", json!({}), json!(2)),
            "{".to_owned(),
            r#"{"jsonrpc":"2.0","method":5,"id":3}"#.to_owned(),
            call("lu_jobs", json!({"limit": "many"}), json!(4)),
            call("no/tool", json!({}), json!(5)),
        ];
        let replies = exchange(&script(&lines), &host).await;
        assert_eq!(replies.len(), 6);
        assert_eq!(replies[1]["error"]["code"], -32601);
        assert_eq!(replies[2]["error"]["code"], -32700);
        assert_eq!(replies[2]["id"], Value::Null);
        assert_eq!(replies[3]["error"]["code"], -32600);
        assert_eq!(replies[4]["error"]["code"], -32602);
        assert_eq!(replies[5]["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn calls_before_initialize_are_refused() {
        let host = Host::failed("unused");
        let lines = vec![
            request("ping", json!({}), json!(1)),
            request("tools/list", json!({}), json!(2)),
            call("lu_status", json!({}), json!(3)),
            initialize("2025-06-18", json!(4)),
            request("ping", json!({}), json!(5)),
        ];
        let replies = exchange(&script(&lines), &host).await;
        assert_eq!(replies.len(), 5);
        for reply in &replies[..3] {
            assert_eq!(reply["error"]["code"], -32002);
        }
        assert_eq!(replies[4]["result"], json!({}));
    }

    #[tokio::test]
    async fn every_tool_has_a_valid_schema_and_honest_annotations() {
        let host = Host::failed("unused");
        let lines = vec![request("tools/list", json!({}), json!(1))];
        let replies = exchange(&script(&initialize_then(&lines)), &host).await;
        let tools = replies[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 13);
        let mut names = Vec::new();
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            names.push(name);
            assert!(!tool["description"].as_str().unwrap().is_empty());
            let read_only = matches!(
                name,
                "lu_status"
                    | "lu_jobs"
                    | "lu_job"
                    | "lu_logs"
                    | "lu_nodes"
                    | "lu_graph"
                    | "lu_result"
            );
            assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{name}");
            let destructive = matches!(name, "lu_cancel" | "lu_drain" | "lu_undrain");
            assert_eq!(
                tool["annotations"].get("destructiveHint"),
                destructive.then_some(&json!(true)),
                "{name}"
            );
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            let properties = schema["properties"].as_object().unwrap();
            let required = schema["required"].as_array().unwrap();
            for key in required {
                assert!(properties.contains_key(key.as_str().unwrap()), "{name}");
            }
            for (key, property) in properties {
                assert!(property["type"].is_string(), "{name}.{key}");
            }
        }
        assert_eq!(
            names,
            vec![
                "lu_status",
                "lu_jobs",
                "lu_job",
                "lu_logs",
                "lu_nodes",
                "lu_graph",
                "lu_submit",
                "lu_submit_many",
                "lu_result",
                "lu_cancel",
                "lu_retry",
                "lu_drain",
                "lu_undrain"
            ]
        );
        let submit_many = tools
            .iter()
            .find(|tool| tool["name"] == "lu_submit_many")
            .unwrap();
        let jobs = &submit_many["inputSchema"]["properties"]["jobs"];
        assert_eq!(jobs["type"], "array");
        assert_eq!(jobs["maxItems"], 12);
        assert_eq!(jobs["items"]["type"], "object");
        assert_eq!(jobs["items"]["additionalProperties"], false);
        assert_eq!(
            jobs["items"]["properties"]["after_names"]["items"]["type"],
            "string"
        );
        assert_eq!(
            jobs["items"]["required"],
            json!(["name", "repo", "ref", "task"])
        );
    }

    fn initialize_then(lines: &[String]) -> Vec<String> {
        let mut all = vec![initialize("2025-06-18", json!(0))];
        all.extend_from_slice(lines);
        all
    }

    #[tokio::test]
    async fn read_tools_return_text_and_structured_content() {
        let (server, id) = fixture().await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let calls = [
            ("lu_status", json!({})),
            ("lu_jobs", json!({"state": "running", "limit": 5})),
            ("lu_job", json!({"id": id})),
            ("lu_logs", json!({"id": id, "lines": 80})),
            ("lu_nodes", json!({})),
            ("lu_graph", json!({})),
        ];
        for (index, (name, args)) in calls.iter().enumerate() {
            let lines = script(&initialize_then(&[call(
                name,
                args.clone(),
                json!(index + 1),
            )]));
            let replies = exchange(&lines, &host).await;
            assert_eq!(replies.len(), 2, "{name}");
            assert_eq!(replies[1]["id"], index + 1, "{name}");
            let (text, structured, failed) = tool_result(&replies[1]);
            assert!(!failed, "{name}: {text}");
            assert!(!text.is_empty(), "{name}");
            assert!(structured.is_object(), "{name}");
        }
    }

    #[tokio::test]
    async fn hostile_network_text_is_sanitised() {
        let (server, id) = fixture().await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        for (name, args) in [
            ("lu_jobs", json!({})),
            ("lu_job", json!({"id": id})),
            ("lu_logs", json!({"id": id})),
        ] {
            let lines = script(&initialize_then(&[call(name, args, json!(1))]));
            let replies = exchange(&lines, &host).await;
            let (text, structured, failed) = tool_result(&replies[1]);
            assert!(!failed, "{name}: {text}");
            assert!(!text.contains("31m"), "{name}");
            assert!(!text.contains('\u{7}'), "{name}");
            assert!(!structured.to_string().contains("31m"), "{name}");
            assert!(
                text.contains("Fix the widget") || name == "lu_logs",
                "{name}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn tool_failures_are_results_not_protocol_errors() {
        let (server, _) = fixture().await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let connected = Host::connected(client);
        let lines = script(&initialize_then(&[call(
            "lu_job",
            json!({"id": "not-a-real-id"}),
            json!(1),
        )]));
        let replies = exchange(&lines, &connected).await;
        let (text, structured, failed) = tool_result(&replies[1]);
        assert!(failed);
        assert!(text.contains("job") || text.contains("id"), "{text}");
        assert!(!text.contains('\n'), "{text}");
        assert_eq!(structured["error"], text);

        let unreachable = Host::failed("cannot reach controller at http://127.0.0.1:1");
        let lines = script(&initialize_then(&[call("lu_status", json!({}), json!(1))]));
        let replies = exchange(&lines, &unreachable).await;
        let (text, _, failed) = tool_result(&replies[1]);
        assert!(failed);
        assert!(text.contains("cannot reach controller"));
    }

    #[tokio::test]
    async fn large_logs_are_capped() {
        let (server, id) = fixture().await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let lines = script(&initialize_then(&[call(
            "lu_logs",
            json!({"id": id}),
            json!(1),
        )]));
        let replies = exchange(&lines, &host).await;
        let (text, structured, failed) = tool_result(&replies[1]);
        assert!(!failed);
        assert!(text.len() < 32 * 1024, "{}", text.len());
        assert!(text.contains("[cut:"), "{text}");
        assert!(structured.to_string().len() < 32 * 1024);
    }

    #[tokio::test]
    async fn stdout_has_only_json_lines() {
        let host = Host::failed("unused");
        let lines = vec![
            initialize("2025-06-18", json!(1)),
            request("ping", json!({}), json!(2)),
        ];
        let script = script(&lines);
        let reader = BufReader::new(script.as_bytes());
        let mut out = Vec::new();
        serve(reader, &mut out, &host).await.unwrap();
        let raw = String::from_utf8(out).unwrap();
        assert_eq!(raw.lines().count(), 2);
        for line in raw.lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            assert!(value.get("jsonrpc").is_some());
        }
    }

    #[tokio::test]
    async fn submit_succeeds_and_derives_stable_keys_from_the_spec() {
        let server = start(|record| {
            if record.path == "/v1/jobs" {
                let spec: Value = serde_json::from_slice(&record.body).unwrap();
                Reply::json(&submitted_job(&spec, "queued"))
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let base = json!({"repo": "/srv/repo", "ref": "main", "task": "fix the widget"});
        let mut changed = base.clone();
        changed["task"] = json!("fix another widget");
        let lines = script(&initialize_then(&[
            call("lu_submit", base.clone(), json!(1)),
            call("lu_submit", base, json!(2)),
            call("lu_submit", changed, json!(3)),
        ]));
        let replies = exchange(&lines, &host).await;
        assert_eq!(replies.len(), 4);
        for reply in &replies[1..] {
            let (text, structured, failed) = tool_result(reply);
            assert!(!failed, "{text}");
            assert_eq!(structured["state"], "queued");
            assert_eq!(structured["id"].as_str().unwrap().len(), 26);
            assert!(text.contains("id="), "{text}");
        }
        let keys = post_keys(&server);
        assert_eq!(keys.len(), 3);
        for key in &keys {
            assert!(key.starts_with("mcp-"), "{key}");
            assert_eq!(key.len(), 20);
            assert!(key.chars().skip(4).all(|c| c.is_ascii_hexdigit()));
        }
        assert_eq!(keys[0], keys[1]);
        assert_ne!(keys[0], keys[2]);
    }

    #[tokio::test]
    async fn submit_reports_controller_errors_and_argument_faults() {
        let server = start(|record| {
            if record.path == "/v1/jobs" {
                Reply::status(400)
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let (text, structured, failed) = tool_result(
            &call_tool(
                &host,
                "lu_submit",
                json!({"repo": "/srv/repo", "ref": "main", "task": "go"}),
            )
            .await,
        );
        assert!(failed);
        assert!(text.contains("400"), "{text}");
        assert_eq!(structured["error"], text);
        assert_eq!(posts(&server), 1);

        let reply = call_tool(
            &host,
            "lu_submit",
            json!({"repo": "/srv/repo", "ref": "main"}),
        )
        .await;
        assert_eq!(reply["error"]["code"], -32602);
        assert!(reply["error"]["message"].as_str().unwrap().contains("task"));
        assert_eq!(posts(&server), 1);
    }

    #[tokio::test]
    async fn submit_many_chains_jobs_in_order() {
        let server = start(|record| {
            if record.path == "/v1/jobs" {
                let spec: Value = serde_json::from_slice(&record.body).unwrap();
                Reply::json(&submitted_job(&spec, "queued"))
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let batch = json!({"jobs": [
            batch_job("a", json!([])),
            batch_job("b", json!(["a"])),
            batch_job("c", json!(["b"]))
        ]});
        let (text, structured, failed) =
            tool_result(&call_tool(&host, "lu_submit_many", batch).await);
        assert!(!failed, "{text}");
        assert_eq!(structured["submitted_count"], 3);
        let jobs = structured["jobs"].as_array().unwrap();
        assert_eq!(jobs[0]["name"], "a");
        assert_eq!(jobs[1]["name"], "b");
        assert_eq!(jobs[2]["name"], "c");
        assert_eq!(structured["not_submitted"], json!([]));

        let bodies = post_bodies(&server);
        assert_eq!(bodies.len(), 3);
        let a_id = jobs[0]["id"].as_str().unwrap();
        let b_id = jobs[1]["id"].as_str().unwrap();
        assert_eq!(bodies[1]["after"], json!([a_id]));
        assert_eq!(bodies[2]["after"], json!([b_id]));
        for body in &bodies {
            assert!(body["idempotency_key"]
                .as_str()
                .unwrap()
                .starts_with("mcp-"));
        }
    }

    #[tokio::test]
    async fn submit_many_rejects_bad_names_before_any_request() {
        let server = start(|_| Reply::json(&job_json(&job_id(), "queued"))).await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let lines = script(&initialize_then(&[
            call(
                "lu_submit_many",
                json!({"jobs": [batch_job("a", json!([])), batch_job("a", json!([]))]}),
                json!(1),
            ),
            call(
                "lu_submit_many",
                json!({"jobs": [batch_job("a", json!(["missing"]))]}),
                json!(2),
            ),
            call(
                "lu_submit_many",
                json!({"jobs": [batch_job("a", json!(["b"])), batch_job("b", json!([]))]}),
                json!(3),
            ),
            call(
                "lu_submit_many",
                json!({"jobs": [batch_job("a", json!(["b"])), batch_job("b", json!(["a"]))]}),
                json!(4),
            ),
        ]));
        let replies = exchange(&lines, &host).await;
        assert_eq!(replies.len(), 5);
        let expected = ["duplicate", "unknown", "before it is defined", "cycle"];
        for (reply, expected) in replies[1..].iter().zip(expected) {
            let (text, structured, failed) = tool_result(reply);
            assert!(failed, "{text}");
            assert!(text.contains(expected), "{text}");
            assert_eq!(structured["error"], text);
        }
        assert_eq!(posts(&server), 0);
    }

    #[tokio::test]
    async fn submit_many_reports_partial_failure() {
        let count = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::clone(&count);
        let server = start(move |record| {
            if record.path == "/v1/jobs" {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt == 2 {
                    return Reply::status(400);
                }
                let spec: Value = serde_json::from_slice(&record.body).unwrap();
                Reply::json(&submitted_job(&spec, "queued"))
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let batch = json!({"jobs": [
            batch_job("a", json!([])),
            batch_job("b", json!(["a"])),
            batch_job("c", json!(["b"]))
        ]});
        let (text, structured, failed) =
            tool_result(&call_tool(&host, "lu_submit_many", batch).await);
        assert!(failed);
        assert!(text.contains("submitted 1 of 3"), "{text}");
        assert_eq!(structured["submitted_count"], 1);
        assert_eq!(structured["not_submitted"], json!(["b", "c"]));
        assert_eq!(structured["submitted"][0]["name"], "a");
        assert_eq!(post_bodies(&server).len(), 2);
    }

    #[tokio::test]
    async fn result_pages_a_large_diff_and_reports_stats() {
        let id = job_id();
        let mut diff = String::from(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n",
        );
        for line in 0..6_000 {
            diff.push_str(&format!("+added line {line}\n"));
        }
        for line in 0..1_400 {
            diff.push_str(&format!("-removed line {line}\n"));
        }
        let total = diff.len();
        assert!(total > 100 * 1024);
        let report = json!({"summary": "all good"}).to_string();
        let mut job = job_json(&id, "done");
        job["summary"] = json!("# finished\nall good");
        let job_path = format!("/v1/jobs/{id}");
        let diff_body = diff.clone();
        let report_body = report.clone();
        let server = start(move |record| {
            if record.path == job_path {
                Reply::json(&job)
            } else if record.path.ends_with("/artifacts/diff.patch") {
                Reply::ok(diff_body.clone())
            } else if record.path.ends_with("/artifacts/report.json") {
                Reply::ok(report_body.clone())
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);

        let mut offset = 0u64;
        let mut assembled = String::new();
        let mut pages = 0;
        loop {
            let (text, structured, failed) = tool_result(
                &call_tool(&host, "lu_result", json!({"id": id, "offset": offset})).await,
            );
            assert!(!failed, "{text}");
            assert_eq!(structured["stats"]["files"], 1);
            assert_eq!(structured["stats"]["added"], 6_000);
            assert_eq!(structured["stats"]["removed"], 1_400);
            assert_eq!(structured["diff_bytes"], total);
            assert_eq!(structured["report"], report);
            let page = structured["diff"].as_str().unwrap().to_owned();
            assert!(!page.is_empty(), "{text}");
            assembled.push_str(&page);
            match structured["next_offset"].as_u64() {
                Some(next) => {
                    assert!(!structured["done"].as_bool().unwrap());
                    assert!(next > offset);
                    offset = next;
                    pages += 1;
                }
                None => {
                    assert!(structured["done"].as_bool().unwrap());
                    break;
                }
            }
            assert!(pages < 20, "paging did not finish");
        }
        assert_eq!(assembled, diff);
        assert!(pages >= 4, "expected several pages, got {pages}");

        let mut offset = 0u64;
        let mut assembled = String::new();
        loop {
            let (text, structured, failed) = tool_result(
                &call_tool(
                    &host,
                    "lu_result",
                    json!({"id": id, "offset": offset, "max_bytes": 32 * 1024}),
                )
                .await,
            );
            assert!(!failed, "{text}");
            assert!(!text.contains("[cut:"), "{text}");
            let page = structured["diff"].as_str().unwrap().to_owned();
            assert!(page.len() <= 32 * 1024, "{}", page.len());
            assembled.push_str(&page);
            match structured["next_offset"].as_u64() {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(assembled, diff);
    }

    #[tokio::test]
    async fn result_reports_unfinished_state_without_artifacts() {
        let id = job_id();
        let job = job_json(&id, "running");
        let job_path = format!("/v1/jobs/{id}");
        let server = start(move |record| {
            if record.path == job_path {
                Reply::json(&job)
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        let (text, structured, failed) =
            tool_result(&call_tool(&host, "lu_result", json!({"id": id})).await);
        assert!(!failed, "{text}");
        assert_eq!(structured["state"], "running");
        assert!(!structured["done"].as_bool().unwrap());
        assert!(text.contains("not ready"), "{text}");
        assert_eq!(server.records.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancel_retry_drain_and_undrain_succeed() {
        let done = job_id();
        let cancel_id = job_id();
        let done_job = job_json(&done, "done");
        let done_path = format!("/v1/jobs/{done}");
        let cancel_path = format!("/v1/jobs/{cancel_id}/cancel");
        let cancelled = job_json(&cancel_id, "cancelled");
        let server = start(move |record| match record.path.as_str() {
            path if path == done_path => Reply::json(&done_job),
            path if path == cancel_path => Reply::json(&cancelled),
            "/v1/jobs" => {
                let spec: Value = serde_json::from_slice(&record.body).unwrap();
                Reply::json(&submitted_job(&spec, "queued"))
            }
            "/v1/nodes/node-a/drain" | "/v1/nodes/node-a/undrain" => Reply::json(&json!({})),
            _ => Reply::status(404),
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);
        for (name, args) in [
            ("lu_cancel", json!({"id": cancel_id})),
            ("lu_retry", json!({"id": done})),
            ("lu_drain", json!({"node": "node-a"})),
            ("lu_undrain", json!({"node": "node-a"})),
        ] {
            let (text, structured, failed) = tool_result(&call_tool(&host, name, args).await);
            assert!(!failed, "{name}: {text}");
            assert!(!text.is_empty(), "{name}");
            assert!(structured.is_object(), "{name}");
        }
    }

    #[tokio::test]
    async fn control_tools_report_errors() {
        let done = job_id();
        let running = job_id();
        let missing = job_id();
        let done_job = job_json(&done, "done");
        let running_job = job_json(&running, "running");
        let done_path = format!("/v1/jobs/{done}");
        let running_path = format!("/v1/jobs/{running}");
        let server = start(move |record| {
            if record.path == done_path {
                Reply::json(&done_job)
            } else if record.path == running_path {
                Reply::json(&running_job)
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let host = Host::connected(client);

        let (text, _, failed) =
            tool_result(&call_tool(&host, "lu_cancel", json!({"id": missing})).await);
        assert!(failed);
        assert!(text.contains("not found"), "{text}");

        let (text, _, failed) =
            tool_result(&call_tool(&host, "lu_result", json!({"id": missing})).await);
        assert!(failed);
        assert!(text.contains("not found"), "{text}");

        let (text, _, failed) =
            tool_result(&call_tool(&host, "lu_retry", json!({"id": running})).await);
        assert!(failed);
        assert!(text.contains("finished"), "{text}");

        for name in ["lu_drain", "lu_undrain"] {
            let (text, _, failed) =
                tool_result(&call_tool(&host, name, json!({"node": "node-missing"})).await);
            assert!(failed, "{name}");
            assert!(text.contains("heartbeat"), "{name}: {text}");
        }
    }

    #[tokio::test]
    async fn submissions_are_limited_per_rolling_minute() {
        let server = start(|record| {
            if record.path == "/v1/jobs" {
                let spec: Value = serde_json::from_slice(&record.body).unwrap();
                Reply::json(&submitted_job(&spec, "queued"))
            } else {
                Reply::status(404)
            }
        })
        .await;
        let client = Client::new(&server.url, TOKEN, None).unwrap();
        let clock = Arc::new(AtomicU64::new(100));
        let now = Arc::clone(&clock);
        let clock_fn: Clock = Arc::new(move || now.load(Ordering::SeqCst));
        let host = Host::connected_at(client, clock_fn);

        let mut lines = initialize_then(&[]);
        for index in 0..31u32 {
            lines.push(call(
                "lu_submit",
                json!({"repo": "/srv/repo", "ref": "main", "task": format!("task {index}")}),
                json!(index + 1),
            ));
        }
        lines.push(call(
            "lu_submit_many",
            json!({"jobs": [batch_job("x", json!([])), batch_job("y", json!([]))]}),
            json!(32),
        ));
        let replies = exchange(&script(&lines), &host).await;
        assert_eq!(replies.len(), 33);
        for reply in &replies[1..31] {
            let (text, _, failed) = tool_result(reply);
            assert!(!failed, "{text}");
        }
        let (text, structured, failed) = tool_result(&replies[31]);
        assert!(failed);
        assert!(text.contains("wait"), "{text}");
        assert!(text.contains("30"), "{text}");
        assert_eq!(structured["error"], text);
        let (text, _, failed) = tool_result(&replies[32]);
        assert!(failed);
        assert!(text.contains("wait"), "{text}");
        assert_eq!(posts(&server), 30);

        clock.store(160, Ordering::SeqCst);
        let (text, _, failed) = tool_result(
            &call_tool(
                &host,
                "lu_submit",
                json!({"repo": "/srv/repo", "ref": "main", "task": "later"}),
            )
            .await,
        );
        assert!(!failed, "{text}");
        assert_eq!(posts(&server), 31);
    }
}
