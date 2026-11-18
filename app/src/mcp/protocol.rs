//! Hand-written JSON-RPC 2.0 layer. It knows nothing about jobs or the controller.

use lu_proto::printable;
use serde_json::{json, Value};

const NEWEST_VERSION: &str = "2025-06-18";
const VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", NEWEST_VERSION];

/// A request id is echoed unchanged; a missing or null id means "notification".
#[derive(Debug)]
pub enum Id {
    Text(String),
    Number(serde_json::Number),
}

impl Id {
    fn value(&self) -> Value {
        match self {
            Self::Text(text) => Value::String(text.clone()),
            Self::Number(number) => Value::Number(number.clone()),
        }
    }
}

#[derive(Debug)]
pub enum Incoming {
    Request {
        method: String,
        params: Value,
        id: Id,
    },
    Notification,
}

#[derive(Debug)]
pub enum Malformed {
    Json,
    Request(Option<Id>),
}

pub fn parse(line: &str) -> Result<Incoming, Malformed> {
    let value: Value = serde_json::from_str(line).map_err(|_| Malformed::Json)?;
    let object = match value.as_object() {
        Some(object) => object,
        None => return Err(Malformed::Request(None)),
    };
    let id = match object.get("id") {
        Some(Value::String(text)) => Some(Id::Text(text.clone())),
        Some(Value::Number(number)) => Some(Id::Number(number.clone())),
        Some(Value::Null) | None => None,
        Some(_) => return Err(Malformed::Request(None)),
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(Malformed::Request(id));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err(Malformed::Request(id));
    };
    let params = match object.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(params @ Value::Object(_)) => params.clone(),
        Some(_) => return Err(Malformed::Request(id)),
    };
    let method = method.to_owned();
    if let Some(id) = id {
        Ok(Incoming::Request { method, params, id })
    } else {
        Ok(Incoming::Notification)
    }
}

pub struct ToolOutput {
    pub text: String,
    pub structured: Value,
}

pub enum ToolError {
    /// A malformed tool argument: the caller gets JSON-RPC -32602.
    Args(String),
    /// A job/controller failure: the caller gets a normal result with `isError: true`.
    Failed(String),
    /// A failure that also reports structured detail, such as a partly submitted batch.
    Detailed { text: String, structured: Value },
}

pub trait ToolHost {
    fn list(&self) -> Value;
    async fn call(&self, name: &str, args: &Value) -> Result<ToolOutput, ToolError>;
}

pub struct Server {
    initialized: bool,
}

impl Server {
    pub fn new() -> Self {
        Self { initialized: false }
    }

    pub async fn handle(&mut self, incoming: Incoming, host: &impl ToolHost) -> Option<Value> {
        let Incoming::Request { method, params, id } = incoming else {
            // Notifications, including notifications/initialized, never get a reply.
            return None;
        };
        let result = self.dispatch(&method, &params, host).await;
        Some(match result {
            Ok(result) => success(id, result),
            Err((code, message)) => error(Some(id), code, message),
        })
    }

    async fn dispatch(
        &mut self,
        method: &str,
        params: &Value,
        host: &impl ToolHost,
    ) -> Result<Value, (i64, String)> {
        let initialized = self.initialized;
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let version = requested
                    .filter(|version| VERSIONS.contains(version))
                    .unwrap_or(NEWEST_VERSION);
                self.initialized = true;
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "lu", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Read, submit and control Lu jobs through the lu_* tools. Read tools do not change state; lu_submit, lu_submit_many, lu_retry, lu_cancel, lu_drain and lu_undrain do. Submissions are limited to 30 per rolling minute."
                }))
            }
            _ if !initialized => Err((-32002, format!("{method} before initialize"))),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": host.list()})),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, "tools/call needs a tool name".to_owned()))?;
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => json!({}),
                    Some(args @ Value::Object(_)) => args.clone(),
                    Some(_) => return Err((-32602, "tool arguments must be an object".to_owned())),
                };
                match host.call(name, &args).await {
                    Ok(output) => Ok(tool_result(output.text, output.structured, false)),
                    Err(ToolError::Failed(reason)) => {
                        Ok(tool_result(reason.clone(), json!({"error": reason}), true))
                    }
                    Err(ToolError::Detailed { text, structured }) => {
                        Ok(tool_result(text, structured, true))
                    }
                    Err(ToolError::Args(reason)) => Err((-32602, reason)),
                }
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        }
    }
}

fn tool_result(text: String, structured: Value, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
        "structuredContent": structured
    })
}

fn success(id: Id, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.value(), "result": result})
}

fn error(id: Option<Id>, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.map(|id| id.value()).unwrap_or(Value::Null),
        "error": {"code": code, "message": printable(&message)}
    })
}

pub fn invalid_request(malformed: Malformed) -> Value {
    match malformed {
        Malformed::Json => error(None, -32700, "parse error".to_owned()),
        Malformed::Request(id) => error(id, -32600, "invalid request".to_owned()),
    }
}
