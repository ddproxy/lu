//! stdio MCP server: JSON-RPC core plus read-only tools over the operator API.

mod format;
mod protocol;
mod tools;

use std::path::Path;
use std::process::ExitCode;
use std::task::{Context, Poll};
use std::{io, io::BufRead, pin::Pin};

use lu_client::Client;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};

use protocol::{ToolError, ToolHost, ToolOutput};

pub(crate) struct Host {
    client: Option<Client>,
    error: Option<String>,
}

impl Host {
    fn connected(client: Client) -> Self {
        Self {
            client: Some(client),
            error: None,
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self {
            client: None,
            error: Some(error.into()),
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
        (
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned(),
            reply["result"]["structuredContent"].clone(),
            reply["result"]["isError"].as_bool().unwrap(),
        )
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
        assert_eq!(replies[2]["result"]["tools"].as_array().unwrap().len(), 6);
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
    async fn every_tool_has_a_valid_read_only_schema() {
        let host = Host::failed("unused");
        let lines = vec![request("tools/list", json!({}), json!(1))];
        let replies = exchange(&script(&initialize_then(&lines)), &host).await;
        let tools = replies[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        let mut names = Vec::new();
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            names.push(name);
            assert!(!tool["description"].as_str().unwrap().is_empty());
            assert_eq!(tool["annotations"], json!({"readOnlyHint": true}));
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
                "lu_graph"
            ]
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
}
