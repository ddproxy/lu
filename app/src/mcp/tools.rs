//! Tool table for `lu mcp`. No transport code lives here.

use std::collections::{HashSet, VecDeque};

use lu_client::{ApiError, Client, JobView, ListNodes};
use lu_proto::{
    truncate_chars, ArtifactName, HarnessState, JobId, JobSpec, JobState, NodeName, ProviderState,
    DEFAULT_TIMEOUT_SECS, MAX_AFTER_JOBS,
};
use lu_tui::graph::{self, Graph};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::format::{
    cap_chars, cap_result, cap_result_with, clean, first_line, one_line, strip_ansi, unix_now,
};
use super::protocol::{ToolError, ToolOutput};
use super::Host;

/// Two lease periods in the default controller configuration, used when `online` is absent.
const STALE_AFTER_SECS: u64 = 120;
/// Submissions allowed by one server process in a rolling minute.
pub(crate) const SUBMIT_LIMIT: usize = 30;
const RATE_WINDOW_SECS: u64 = 60;
const SUBMIT_MANY_LIMIT: usize = 12;
const RESULT_MAX_BYTES: usize = 32 * 1024;
const RESULT_DEFAULT_BYTES: usize = 24 * 1024;
const REPORT_CHARS: usize = 4_000;

pub(crate) struct RateLimiter {
    recent: VecDeque<u64>,
    limit: usize,
}

impl RateLimiter {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            recent: VecDeque::new(),
            limit,
        }
    }

    fn prune(&mut self, now: u64) {
        while let Some(&oldest) = self.recent.front() {
            if now.saturating_sub(oldest) < RATE_WINDOW_SECS {
                break;
            }
            self.recent.pop_front();
        }
    }

    pub(crate) fn reserve(&mut self, count: usize, now: u64) -> Result<(), u64> {
        self.prune(now);
        if self.recent.len().saturating_add(count) > self.limit {
            let oldest = self.recent.front().copied().unwrap_or(now);
            return Err(oldest
                .saturating_add(RATE_WINDOW_SECS)
                .saturating_sub(now)
                .max(1));
        }
        for _ in 0..count {
            self.recent.push_back(now);
        }
        Ok(())
    }
}

struct Field {
    key: &'static str,
    kind: &'static str,
    description: &'static str,
    required: bool,
    default_int: Option<i64>,
    default_str: Option<&'static str>,
    min: Option<i64>,
    max: Option<i64>,
    item: Option<&'static Field>,
    fields: Option<&'static [Field]>,
}

const fn required(key: &'static str, kind: &'static str, description: &'static str) -> Field {
    Field {
        key,
        kind,
        description,
        required: true,
        default_int: None,
        default_str: None,
        min: None,
        max: None,
        item: None,
        fields: None,
    }
}

const fn optional(key: &'static str, kind: &'static str, description: &'static str) -> Field {
    Field {
        key,
        kind,
        description,
        required: false,
        default_int: None,
        default_str: None,
        min: None,
        max: None,
        item: None,
        fields: None,
    }
}

const fn bounded(mut field: Field, min: i64, max: i64, default: i64) -> Field {
    field.min = Some(min);
    field.max = Some(max);
    field.default_int = Some(default);
    field
}

const fn limited(mut field: Field, min: i64, max: i64) -> Field {
    field.min = Some(min);
    field.max = Some(max);
    field
}

const fn array(
    key: &'static str,
    description: &'static str,
    item: &'static Field,
    required: bool,
    min: i64,
    max: i64,
) -> Field {
    Field {
        key,
        kind: "array",
        description,
        required,
        default_int: None,
        default_str: None,
        min: Some(min),
        max: Some(max),
        item: Some(item),
        fields: None,
    }
}

const fn object_item(fields: &'static [Field]) -> Field {
    Field {
        key: "",
        kind: "object",
        description: "",
        required: false,
        default_int: None,
        default_str: None,
        min: None,
        max: None,
        item: None,
        fields: Some(fields),
    }
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    fields: &'static [Field],
    read_only: bool,
    destructive: bool,
}

const ID_FIELD: Field = required("id", "string", "Job id");
static STRING_ITEM: Field = required("", "string", "");
const AFTER_NAMES_FIELD: Field = array(
    "after_names",
    "Names of earlier jobs in this call",
    &STRING_ITEM,
    false,
    0,
    MAX_AFTER_JOBS as i64,
);
static JOB_FIELDS: &[Field] = &[
    required("name", "string", "Name used by later after_names entries"),
    required("repo", "string", "Repository URL or absolute local path"),
    required("ref", "string", "Git ref to check out"),
    required("task", "string", "Task text, never a path"),
    optional("provider", "string", "Provider name"),
    optional("model", "string", "Model name"),
    bounded(
        optional("timeout_secs", "integer", "Job timeout in seconds"),
        1,
        86_400,
        DEFAULT_TIMEOUT_SECS as i64,
    ),
    limited(
        optional(
            "idle_timeout_secs",
            "integer",
            "Stop the harness after this many seconds without output",
        ),
        1,
        86_400,
    ),
    optional(
        "key",
        "string",
        "Idempotency key; derived from the spec when absent",
    ),
    AFTER_NAMES_FIELD,
];
static JOB_ITEM: Field = object_item(JOB_FIELDS);
const AFTER_FIELD: Field = array(
    "after",
    "Job ids that must finish first",
    &STRING_ITEM,
    false,
    0,
    MAX_AFTER_JOBS as i64,
);
const AFTER_KEYS_FIELD: Field = array(
    "after_keys",
    "Idempotency keys of jobs that must finish first",
    &STRING_ITEM,
    false,
    0,
    MAX_AFTER_JOBS as i64,
);

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "lu_status",
        description: "Controller version, uptime, queue and node counts.",
        fields: &[],
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "lu_jobs",
        description: "Compact list of jobs, newest first.",
        fields: &[
            optional("state", "string", "Filter by job state"),
            bounded(
                optional("limit", "integer", "Maximum jobs, at most 100"),
                1,
                100,
                20,
            ),
        ],
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "lu_job",
        description: "Full detail for one job, including its spec and artifacts.",
        fields: &[ID_FIELD],
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "lu_logs",
        description: "The tail of a job's log with escape sequences removed.",
        fields: &[
            ID_FIELD,
            bounded(
                optional("lines", "integer", "Tail lines, at most 400"),
                1,
                400,
                80,
            ),
        ],
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "lu_nodes",
        description: "Node states, providers, load and cordons.",
        fields: &[],
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "lu_graph",
        description: "Dependency graph of unfinished jobs.",
        fields: &[],
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "lu_submit",
        description: "Submit one job. A key is derived from the spec when none is given.",
        fields: &[
            required("repo", "string", "Repository URL or absolute local path"),
            required("ref", "string", "Git ref to check out"),
            required("task", "string", "Task text, never a path"),
            optional("provider", "string", "Provider name"),
            optional("model", "string", "Model name"),
            AFTER_FIELD,
            AFTER_KEYS_FIELD,
            bounded(
                optional("timeout_secs", "integer", "Job timeout in seconds"),
                1,
                86_400,
                DEFAULT_TIMEOUT_SECS as i64,
            ),
            limited(
                optional(
                    "idle_timeout_secs",
                    "integer",
                    "Stop the harness after this many seconds without output",
                ),
                1,
                86_400,
            ),
            optional(
                "key",
                "string",
                "Idempotency key; derived from the spec when absent",
            ),
        ],
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "lu_submit_many",
        description: "Submit up to 12 jobs in order, chaining them by name.",
        fields: &[array(
            "jobs",
            "Jobs to submit, in order, at most 12",
            &JOB_ITEM,
            true,
            1,
            SUBMIT_MANY_LIMIT as i64,
        )],
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "lu_result",
        description: "A finished job's summary, report, diff stats and paged diff text.",
        fields: &[
            ID_FIELD,
            bounded(
                optional("offset", "integer", "Byte offset into the diff"),
                0,
                i64::MAX,
                0,
            ),
            bounded(
                optional(
                    "max_bytes",
                    "integer",
                    "Diff bytes to return, at most 32 KiB",
                ),
                1,
                RESULT_MAX_BYTES as i64,
                RESULT_DEFAULT_BYTES as i64,
            ),
        ],
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "lu_cancel",
        description: "Ask the controller to cancel a job.",
        fields: &[ID_FIELD],
        read_only: false,
        destructive: true,
    },
    ToolDef {
        name: "lu_retry",
        description: "Resubmit a finished job's spec as a new job.",
        fields: &[
            ID_FIELD,
            optional("provider", "string", "Replace the provider name"),
            optional("model", "string", "Replace the model name"),
            AFTER_FIELD,
            optional("key", "string", "Idempotency key for the new job"),
        ],
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "lu_drain",
        description: "Stop assigning new jobs to a node.",
        fields: &[required("node", "string", "Node name")],
        read_only: false,
        destructive: true,
    },
    ToolDef {
        name: "lu_undrain",
        description: "Allow a drained node to take jobs again.",
        fields: &[required("node", "string", "Node name")],
        read_only: false,
        destructive: true,
    },
];

fn field_schema(field: &Field) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_owned(), Value::String(field.kind.to_owned()));
    if !field.description.is_empty() {
        schema.insert(
            "description".to_owned(),
            Value::String(field.description.to_owned()),
        );
    }
    if let Some(min) = field.min {
        let key = if field.kind == "array" {
            "minItems"
        } else {
            "minimum"
        };
        schema.insert(key.to_owned(), json!(min));
    }
    if let Some(max) = field.max {
        let key = if field.kind == "array" {
            "maxItems"
        } else {
            "maximum"
        };
        schema.insert(key.to_owned(), json!(max));
    }
    if let Some(default) = field.default_int {
        schema.insert("default".to_owned(), json!(default));
    }
    if let Some(item) = field.item {
        schema.insert("items".to_owned(), field_schema(item));
    }
    if let Some(fields) = field.fields {
        let (properties, required) = object_schema(fields);
        schema.insert("properties".to_owned(), Value::Object(properties));
        schema.insert("required".to_owned(), Value::Array(required));
        schema.insert("additionalProperties".to_owned(), Value::Bool(false));
    }
    Value::Object(schema)
}

fn object_schema(fields: &[Field]) -> (Map<String, Value>, Vec<Value>) {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in fields {
        properties.insert(field.key.to_owned(), field_schema(field));
        if field.required {
            required.push(Value::String(field.key.to_owned()));
        }
    }
    (properties, required)
}

pub fn list() -> Value {
    let tools = TOOLS
        .iter()
        .map(|tool| {
            let (properties, required) = object_schema(tool.fields);
            let mut annotations = Map::new();
            annotations.insert("readOnlyHint".to_owned(), json!(tool.read_only));
            if tool.destructive {
                annotations.insert("destructiveHint".to_owned(), json!(true));
            }
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": {
                    "type": "object",
                    "properties": Value::Object(properties),
                    "required": required,
                    "additionalProperties": false
                },
                "annotations": Value::Object(annotations)
            })
        })
        .collect::<Vec<_>>();
    Value::Array(tools)
}

fn validate_value(field: &Field, value: &Value, label: &str) -> Result<Value, ToolError> {
    match field.kind {
        "string" => value
            .as_str()
            .map(|text| Value::String(text.to_owned()))
            .ok_or_else(|| ToolError::Args(format!("{label} must be a string"))),
        "integer" => {
            let number = value
                .as_i64()
                .ok_or_else(|| ToolError::Args(format!("{label} must be an integer")))?;
            if let Some(min) = field.min {
                if number < min {
                    return Err(ToolError::Args(format!("{label} must be at least {min}")));
                }
            }
            if let Some(max) = field.max {
                if number > max {
                    return Err(ToolError::Args(format!("{label} must be at most {max}")));
                }
            }
            Ok(json!(number))
        }
        "array" => {
            let items = value
                .as_array()
                .ok_or_else(|| ToolError::Args(format!("{label} must be an array")))?;
            if let Some(min) = field.min {
                if (items.len() as i64) < min {
                    return Err(ToolError::Args(format!(
                        "{label} must have at least {min} items"
                    )));
                }
            }
            if let Some(max) = field.max {
                if items.len() as i64 > max {
                    return Err(ToolError::Args(format!(
                        "{label} must have at most {max} items"
                    )));
                }
            }
            let item = field.item.expect("array fields declare an item schema");
            let mut out = Vec::with_capacity(items.len());
            for (index, value) in items.iter().enumerate() {
                out.push(validate_value(item, value, &format!("{label}[{index}]"))?);
            }
            Ok(Value::Array(out))
        }
        "object" => validate_fields(
            field.fields.expect("object fields declare their members"),
            value,
            label,
        ),
        _ => unreachable!("tool fields are declared as string, integer, array or object"),
    }
}

fn validate_fields(fields: &[Field], args: &Value, at: &str) -> Result<Value, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| ToolError::Args("tool arguments must be an object".to_owned()))?;
    for key in object.keys() {
        if !fields.iter().any(|field| field.key == key) {
            let label = if at.is_empty() {
                clean(key)
            } else {
                format!("{at}.{}", clean(key))
            };
            return Err(ToolError::Args(format!("unknown argument: {label}")));
        }
    }
    let mut out = Map::new();
    for field in fields {
        let label = if at.is_empty() {
            field.key.to_owned()
        } else {
            format!("{at}.{}", field.key)
        };
        match object.get(field.key) {
            Some(value) => {
                out.insert(field.key.to_owned(), validate_value(field, value, &label)?);
            }
            None if field.required => {
                return Err(ToolError::Args(format!("missing argument: {label}")));
            }
            None => {
                if let Some(default) = field.default_int {
                    out.insert(field.key.to_owned(), json!(default));
                } else if let Some(default) = field.default_str {
                    out.insert(field.key.to_owned(), Value::String(default.to_owned()));
                }
            }
        }
    }
    Ok(Value::Object(out))
}

fn validate(def: &ToolDef, args: &Value) -> Result<Value, ToolError> {
    validate_fields(def.fields, args, "")
}

pub async fn call(host: &Host, name: &str, args: &Value) -> Result<ToolOutput, ToolError> {
    let def = TOOLS
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| ToolError::Args(format!("unknown tool: {}", clean(name))))?;
    let args = validate(def, args)?;
    if name == "lu_result" {
        return result(host, &args)
            .await
            .map(|output| cap_result_with(output, RESULT_MAX_BYTES));
    }
    let output = match name {
        "lu_status" => status(host).await?,
        "lu_jobs" => jobs(host, &args).await?,
        "lu_job" => job(host, &args).await?,
        "lu_logs" => logs(host, &args).await?,
        "lu_nodes" => nodes(host).await?,
        "lu_graph" => graph(host).await?,
        "lu_submit" => submit(host, &args).await?,
        "lu_submit_many" => submit_many(host, &args).await?,
        "lu_cancel" => cancel(host, &args).await?,
        "lu_retry" => retry(host, &args).await?,
        "lu_drain" => drain(host, &args, true).await?,
        "lu_undrain" => drain(host, &args, false).await?,
        _ => unreachable!("tool table and dispatch agree"),
    };
    Ok(cap_result(output))
}

fn connected(host: &Host) -> Result<&Client, ToolError> {
    host.client.as_ref().ok_or_else(|| {
        ToolError::Failed(clean(
            host.error
                .as_deref()
                .unwrap_or("controller connection is not available"),
        ))
    })
}

fn failure(error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(one_line(&error.to_string()))
}

fn job_id(args: &Value) -> Result<JobId, ToolError> {
    let text = args["id"].as_str().unwrap_or_default();
    JobId::parse(text).map_err(|error| ToolError::Failed(format!("bad job id: {error}")))
}

fn field_strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn parse_ids(values: &[String]) -> Result<Vec<JobId>, ToolError> {
    let mut out = Vec::new();
    for value in values {
        let id = JobId::parse(value)
            .map_err(|error| ToolError::Failed(format!("bad job id: {error}")))?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

fn reserve(host: &Host, count: usize) -> Result<(), ToolError> {
    let now = (host.clock)();
    let mut limiter = host
        .limiter
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    limiter.reserve(count, now).map_err(|wait| {
        ToolError::Failed(format!(
            "submission limit reached: at most {SUBMIT_LIMIT} submissions per rolling minute; wait {wait} seconds"
        ))
    })
}

async fn resolve_after(
    client: &Client,
    ids: &[JobId],
    keys: &[String],
) -> Result<Vec<JobId>, ToolError> {
    let mut out = ids.to_vec();
    for key in keys {
        let Some(view) = client.job_by_key(key).await.map_err(failure)? else {
            return Err(ToolError::Failed(format!(
                "no job with idempotency key {:?}",
                clean(key)
            )));
        };
        if !out.contains(&view.id) {
            out.push(view.id);
        }
    }
    Ok(out)
}

fn build_spec(args: &Value, after: Vec<JobId>) -> Result<JobSpec, ToolError> {
    let timeout = args
        .get("timeout_secs")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS as i64) as u64;
    let spec = JobSpec {
        repo: args["repo"].as_str().unwrap_or_default().to_owned(),
        git_ref: args["ref"].as_str().unwrap_or_default().to_owned(),
        base_commit: None,
        task: args["task"].as_str().unwrap_or_default().to_owned(),
        harness: "opencode".to_owned(),
        provider: optional_string(args, "provider"),
        model: optional_string(args, "model"),
        timeout_secs: timeout,
        idle_timeout_secs: args.get("idle_timeout_secs").and_then(Value::as_u64),
        idempotency_key: optional_string(args, "key"),
        after,
    };
    spec.validate()
        .map_err(|error| ToolError::Failed(format!("invalid job spec: {error}")))?;
    Ok(spec)
}

fn derived_key(spec: &JobSpec) -> String {
    let mut plain = spec.clone();
    plain.idempotency_key = None;
    let digest = Sha256::digest(plain.canonical_json().as_bytes());
    let mut key = String::from("mcp-");
    for byte in &digest[..8] {
        key.push_str(&format!("{byte:02x}"));
    }
    key
}

fn graph_position(view: &JobView) -> String {
    let after = view
        .after
        .iter()
        .map(|id| clean(id.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let waiting = view
        .waiting_on
        .iter()
        .map(|id| clean(id.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{} {} after={} waiting_on={}",
        clean(view.id.as_str()),
        crate::operator::state_name(view.state),
        after,
        waiting
    )
}

fn submit_output(view: &JobView) -> ToolOutput {
    let title = clean(&crate::operator::job_title(&view.spec.task));
    let position = graph_position(view);
    let state = crate::operator::state_name(view.state);
    let node = view.node.as_ref().map(|node| clean(node.as_str()));
    let after = view
        .after
        .iter()
        .map(|id| clean(id.as_str()))
        .collect::<Vec<_>>();
    let waiting_on = view
        .waiting_on
        .iter()
        .map(|id| clean(id.as_str()))
        .collect::<Vec<_>>();
    let text = format!(
        "id={} state={} attempt={} node={} repo={} ref={} title={}\nafter={} waiting_on={}\ngraph: {}",
        clean(view.id.as_str()),
        state,
        view.attempt,
        node.as_deref().unwrap_or("-"),
        clean(&view.spec.repo),
        clean(&view.spec.git_ref),
        title,
        after.join(","),
        waiting_on.join(","),
        position
    );
    ToolOutput {
        text,
        structured: json!({
            "id": clean(view.id.as_str()),
            "state": state,
            "attempt": view.attempt,
            "node": node,
            "created_at": view.created_at,
            "failure": crate::operator::failure_name(view.failure_class),
            "spec": {
                "repo": clean(&view.spec.repo),
                "ref": clean(&view.spec.git_ref),
                "task": cap_chars(&clean(&view.spec.task), 2000),
                "provider": view.spec.provider.as_deref().map(clean),
                "model": view.spec.model.as_deref().map(clean),
                "timeout_secs": view.spec.timeout_secs,
                "idle_timeout_secs": view.spec.idle_timeout_secs,
                "idempotency_key": view.spec.idempotency_key.as_deref().map(clean)
            },
            "after": after,
            "waiting_on": waiting_on,
            "graph": position
        }),
    }
}

async fn submit(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    reserve(host, 1)?;
    let client = connected(host)?;
    let ids = parse_ids(&field_strings(args, "after"))?;
    let keys = field_strings(args, "after_keys");
    let after = resolve_after(client, &ids, &keys).await?;
    let mut spec = build_spec(args, after)?;
    if spec.idempotency_key.is_none() {
        spec.idempotency_key = Some(derived_key(&spec));
    }
    let view = client.submit(&spec).await.map_err(failure)?;
    Ok(submit_output(&view))
}

struct BatchEntry {
    name: String,
    after_names: Vec<String>,
    spec: JobSpec,
}

fn check_cycle(
    entries: &[BatchEntry],
    index: &std::collections::HashMap<&str, usize>,
    state: &mut [u8],
    at: usize,
) -> Result<(), String> {
    state[at] = 1;
    for name in &entries[at].after_names {
        let next = index[name.as_str()];
        match state[next] {
            1 => return Err(format!("cycle through job name {:?}", clean(name))),
            0 => check_cycle(entries, index, state, next)?,
            _ => {}
        }
    }
    state[at] = 2;
    Ok(())
}

fn preflight(entries: &[BatchEntry]) -> Result<(), ToolError> {
    let mut seen = HashSet::new();
    for entry in entries {
        if entry.name.is_empty() {
            return Err(ToolError::Failed("job name must not be empty".to_owned()));
        }
        if !seen.insert(entry.name.as_str()) {
            return Err(ToolError::Failed(format!(
                "duplicate job name: {}",
                clean(&entry.name)
            )));
        }
    }
    let index = entries
        .iter()
        .enumerate()
        .map(|(at, entry)| (entry.name.as_str(), at))
        .collect::<std::collections::HashMap<_, _>>();
    for entry in entries {
        for name in &entry.after_names {
            if !index.contains_key(name.as_str()) {
                return Err(ToolError::Failed(format!(
                    "unknown job name: {}",
                    clean(name)
                )));
            }
        }
    }
    let mut state = vec![0u8; entries.len()];
    for at in 0..entries.len() {
        if state[at] == 0 {
            check_cycle(entries, &index, &mut state, at).map_err(ToolError::Failed)?;
        }
    }
    let mut defined = HashSet::new();
    for entry in entries {
        for name in &entry.after_names {
            if !defined.contains(name.as_str()) {
                return Err(ToolError::Failed(format!(
                    "job name {} is used before it is defined",
                    clean(name)
                )));
            }
        }
        defined.insert(entry.name.as_str());
    }
    Ok(())
}

async fn submit_many(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    let jobs = args["jobs"]
        .as_array()
        .expect("the jobs argument is an array");
    let mut entries = Vec::with_capacity(jobs.len());
    for entry in jobs {
        let name = entry["name"].as_str().unwrap_or_default().to_owned();
        let after_names = field_strings(entry, "after_names");
        let spec = build_spec(entry, Vec::new())?;
        entries.push(BatchEntry {
            name,
            after_names,
            spec,
        });
    }
    preflight(&entries)?;
    reserve(host, entries.len())?;
    let client = connected(host)?;
    let mut ids_by_name: std::collections::HashMap<String, JobId> =
        std::collections::HashMap::new();
    let mut submitted = Vec::new();
    let mut lines = Vec::new();
    for (at, entry) in entries.iter().enumerate() {
        let mut after = Vec::new();
        for name in &entry.after_names {
            let id = ids_by_name
                .get(name.as_str())
                .expect("preflight resolved every after_names entry")
                .clone();
            if !after.contains(&id) {
                after.push(id);
            }
        }
        let mut spec = entry.spec.clone();
        spec.after = after;
        if spec.idempotency_key.is_none() {
            spec.idempotency_key = Some(derived_key(&spec));
        }
        spec.validate()
            .map_err(|error| ToolError::Failed(format!("invalid job spec: {error}")))?;
        match client.submit(&spec).await {
            Ok(view) => {
                ids_by_name.insert(entry.name.clone(), view.id.clone());
                let output = submit_output(&view);
                lines.push(format!(
                    "submitted name={} id={} state={} graph={}",
                    clean(&entry.name),
                    clean(view.id.as_str()),
                    crate::operator::state_name(view.state),
                    output.structured["graph"].as_str().unwrap_or_default()
                ));
                submitted.push(json!({
                    "name": clean(&entry.name),
                    "id": clean(view.id.as_str()),
                    "state": crate::operator::state_name(view.state),
                    "after": view.after.iter().map(|id| clean(id.as_str())).collect::<Vec<_>>(),
                    "graph": output.structured["graph"]
                }));
            }
            Err(error) => {
                let reason = one_line(&error.to_string());
                let not_submitted = entries[at..]
                    .iter()
                    .map(|entry| clean(&entry.name))
                    .collect::<Vec<_>>();
                let text = format!(
                    "submitted {} of {} jobs; not submitted: {}; error: {reason}",
                    submitted.len(),
                    entries.len(),
                    not_submitted.join(", ")
                );
                return Err(ToolError::Detailed {
                    text,
                    structured: json!({
                        "error": reason,
                        "submitted": submitted,
                        "not_submitted": not_submitted,
                        "submitted_count": submitted.len(),
                        "not_submitted_count": not_submitted.len()
                    }),
                });
            }
        }
    }
    Ok(ToolOutput {
        text: lines.join("\n"),
        structured: json!({
            "jobs": submitted,
            "submitted_count": submitted.len(),
            "not_submitted": Vec::<String>::new()
        }),
    })
}

fn diff_stats(diff: &str) -> (usize, usize, usize) {
    let mut files = 0;
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            files += 1;
        } else if line.starts_with("+++") || line.starts_with("---") {
        } else if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (files, added, removed)
}

fn diff_page(diff: &str, offset: u64, max_bytes: usize) -> (String, u64, Option<u64>) {
    let total = diff.len();
    let mut start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    while start < total && !diff.is_char_boundary(start) {
        start += 1;
    }
    let mut end = start.saturating_add(max_bytes).min(total);
    while end > start && !diff.is_char_boundary(end) {
        end -= 1;
    }
    let page = clean(&diff[start..end]);
    let next = if end < total { Some(end as u64) } else { None };
    (page, end as u64, next)
}

async fn artifact(client: &Client, id: &JobId, name: ArtifactName) -> Result<String, ToolError> {
    match client.fetch_artifact(id, name).await {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(ApiError::NotFound) => Ok(String::new()),
        Err(error) => Err(failure(error)),
    }
}

async fn result(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    let id = job_id(args)?;
    let offset = args["offset"].as_u64().unwrap_or(0);
    let max_bytes = args["max_bytes"]
        .as_u64()
        .unwrap_or(RESULT_DEFAULT_BYTES as u64) as usize;
    let client = connected(host)?;
    let view = client.get_job(&id).await.map_err(failure)?;
    let state = crate::operator::state_name(view.state);
    let summary = clean(view.summary.as_deref().unwrap_or_default());
    let node = view.node.as_ref().map(|node| clean(node.as_str()));
    if !view.state.is_terminal() {
        let text = format!(
            "job {} is {state}; the result is not ready yet",
            clean(id.as_str())
        );
        return Ok(ToolOutput {
            text,
            structured: json!({
                "id": clean(id.as_str()),
                "state": state,
                "attempt": view.attempt,
                "node": node,
                "summary": cap_chars(&summary, 2000),
                "done": false
            }),
        });
    }
    let report = cap_chars(
        &clean(&artifact(client, &id, ArtifactName::ReportJson).await?),
        REPORT_CHARS,
    );
    let diff = artifact(client, &id, ArtifactName::DiffPatch).await?;
    let (files, added, removed) = diff_stats(&diff);
    let total = diff.len();
    let summary = cap_chars(&summary, 2000);
    let mut limit = max_bytes.min(RESULT_MAX_BYTES);
    loop {
        let (page, end, next) = diff_page(&diff, offset, limit);
        let tail = match next {
            Some(next) => format!(
                "more remains: {} of {total} diff bytes; call lu_result with offset={next}",
                total.saturating_sub(end as usize)
            ),
            None => "done: no more diff".to_owned(),
        };
        let text = format!(
            "id={} state={} attempt={} node={}\nsummary:\n{}\nreport:\n{}\ndiff stats: files={files} added={added} removed={removed}\ndiff:\n{}\n{tail}",
            clean(id.as_str()),
            state,
            view.attempt,
            node.as_deref().unwrap_or("-"),
            summary,
            report,
            if page.is_empty() {
                "[no diff at this offset]"
            } else {
                &page
            }
        );
        let structured = json!({
            "id": clean(id.as_str()),
            "state": state,
            "attempt": view.attempt,
            "node": &node,
            "summary": &summary,
            "report": &report,
            "stats": {"files": files, "added": added, "removed": removed},
            "diff": &page,
            "offset": offset,
            "next_offset": next,
            "done": next.is_none(),
            "diff_bytes": total
        });
        let json_bytes = serde_json::to_string(&structured)
            .map(|text| text.len())
            .unwrap_or(usize::MAX);
        let text_bytes = serde_json::to_string(&text)
            .map(|text| text.len())
            .unwrap_or(usize::MAX);
        let over = text_bytes
            .saturating_sub(RESULT_MAX_BYTES)
            .max(json_bytes.saturating_sub(RESULT_MAX_BYTES));
        if over == 0 || limit <= 1 {
            return Ok(ToolOutput { text, structured });
        }
        // Shrink the page until the whole result, not just the diff, fits the cap: a
        // truncated page would make next_offset skip bytes the caller never saw.
        limit = limit.saturating_sub(over).max(1);
    }
}

async fn cancel(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    let id = job_id(args)?;
    let view = connected(host)?.cancel(&id).await.map_err(failure)?;
    Ok(submit_output(&view))
}

async fn retry(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    reserve(host, 1)?;
    let id = job_id(args)?;
    let client = connected(host)?;
    let view = client.get_job(&id).await.map_err(failure)?;
    if !view.state.is_terminal() {
        return Err(ToolError::Failed(format!(
            "job {} is {}; retry resubmits a finished job",
            clean(id.as_str()),
            crate::operator::state_name(view.state)
        )));
    }
    let mut spec = view.spec;
    if let Some(provider) = optional_string(args, "provider") {
        spec.provider = Some(provider);
    }
    if let Some(model) = optional_string(args, "model") {
        spec.model = Some(model);
    }
    if args.get("after").is_some() {
        spec.after = parse_ids(&field_strings(args, "after"))?;
    }
    spec.idempotency_key = optional_string(args, "key");
    spec.validate()
        .map_err(|error| ToolError::Failed(format!("invalid job spec: {error}")))?;
    let view = client.submit(&spec).await.map_err(failure)?;
    Ok(submit_output(&view))
}

async fn drain(host: &Host, args: &Value, drain: bool) -> Result<ToolOutput, ToolError> {
    let text = args["node"].as_str().unwrap_or_default();
    let node = NodeName::parse(text)
        .map_err(|error| ToolError::Failed(format!("bad node name: {error}")))?;
    let view = match connected(host)?.set_drain(&node, drain).await {
        Ok(view) => view,
        Err(ApiError::NotFound) => {
            return Err(ToolError::Failed(format!(
                "node {:?} not found: a node must heartbeat at least once before it can be drained or undrained",
                clean(node.as_str())
            )))
        }
        Err(error) => return Err(failure(error)),
    };
    let action = if drain { "drained" } else { "undrained" };
    Ok(ToolOutput {
        text: format!("{} {action}", clean(view.name.as_str())),
        structured: json!({
            "name": clean(view.name.as_str()),
            "draining": view.draining,
            "action": action
        }),
    })
}

async fn status(host: &Host) -> Result<ToolOutput, ToolError> {
    let status = connected(host)?.status().await.map_err(failure)?;
    let jobs: Map<String, Value> = status
        .jobs
        .iter()
        .map(|(name, count)| (clean(name), json!(count)))
        .collect();
    let queue = jobs
        .iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect::<Vec<_>>()
        .join(" ");
    let text = format!(
        "version {} uptime_secs={} schema_version={} db_bytes={} recovered_on_boot={} jobs=[{queue}] oldest_queued_secs={} nodes={} nodes_online={} lease_secs={}",
        clean(&status.version),
        status.uptime_secs,
        status.schema_version,
        status.db_bytes,
        status.recovered_on_boot,
        status.oldest_queued_secs,
        status.nodes,
        status.nodes_online,
        status.lease_secs
    );
    Ok(ToolOutput {
        text,
        structured: json!({
            "version": clean(&status.version),
            "uptime_secs": status.uptime_secs,
            "schema_version": status.schema_version,
            "db_bytes": status.db_bytes,
            "recovered_on_boot": status.recovered_on_boot,
            "jobs": jobs,
            "oldest_queued_secs": status.oldest_queued_secs,
            "nodes": status.nodes,
            "nodes_online": status.nodes_online,
            "lease_secs": status.lease_secs
        }),
    })
}

async fn jobs(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    let state = match args.get("state").and_then(Value::as_str) {
        Some(name) => Some(
            serde_json::from_str::<JobState>(&json!(name).to_string())
                .map_err(|_| ToolError::Args(format!("invalid state: {}", clean(name))))?,
        ),
        None => None,
    };
    let limit = args["limit"].as_i64().unwrap_or(20) as u32;
    let page = connected(host)?
        .list_jobs(state, Some(limit))
        .await
        .map_err(failure)?;
    let now = unix_now();
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for view in &page.jobs {
        let summary = truncate_chars(
            &view.summary.as_deref().map(first_line).unwrap_or_default(),
            120,
        );
        let row = json!({
            "id": clean(view.id.as_str()),
            "state": crate::operator::state_name(view.state),
            "node": view.node.as_ref().map(|node| clean(node.as_str())),
            "attempt": view.attempt,
            "age_secs": now.saturating_sub(view.created_at),
            "title": clean(&crate::operator::job_title(&view.spec.task)),
            "failure": crate::operator::failure_name(view.failure_class),
            "summary": summary
        });
        lines.push(format!(
            "{} {} node={} attempt={} age={}s title={} failure={} summary={}",
            row["id"],
            row["state"],
            row["node"],
            row["attempt"],
            row["age_secs"],
            row["title"],
            row["failure"],
            row["summary"]
        ));
        rows.push(row);
    }
    Ok(ToolOutput {
        text: lines.join("\n"),
        structured: json!({"jobs": rows, "next": page.next.as_ref().map(|next| clean(next))}),
    })
}

async fn job(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    let id = job_id(args)?;
    let raw = connected(host)?.get_job_raw(&id).await.map_err(failure)?;
    let body: Value = serde_json::from_str(&raw).map_err(|error| {
        ToolError::Failed(one_line(&format!("bad controller job reply: {error}")))
    })?;
    let view: JobView = serde_json::from_value(body.clone()).map_err(|error| {
        ToolError::Failed(one_line(&format!("bad controller job reply: {error}")))
    })?;
    let artifacts = body
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(clean)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let task = cap_chars(&clean(&view.spec.task), 2000);
    let summary = strip_ansi(view.summary.as_deref().unwrap_or_default());
    let state = crate::operator::state_name(view.state);
    let failure = crate::operator::failure_name(view.failure_class);
    let node = view.node.as_ref().map(|node| clean(node.as_str()));
    let after = view
        .after
        .iter()
        .map(|id| clean(id.as_str()))
        .collect::<Vec<_>>();
    let waiting_on = view
        .waiting_on
        .iter()
        .map(|id| clean(id.as_str()))
        .collect::<Vec<_>>();
    let text = format!(
        "id={} state={} attempt={} node={} epoch={}\ncreated_at={} started_at={} finished_at={}\nfailure={} exit_code={}\nrepo={} ref={} harness={} provider={} model={} timeout_secs={} idle_timeout_secs={}\nafter={} waiting_on={} artifacts={}\ntask:\n{task}\nsummary:\n{summary}",
        clean(view.id.as_str()),
        state,
        view.attempt,
        node.as_deref().unwrap_or("-"),
        view.epoch,
        view.created_at,
        view.started_at.unwrap_or_default(),
        view.finished_at.unwrap_or_default(),
        failure,
        body.get("exit_code").unwrap_or(&Value::Null),
        clean(&view.spec.repo),
        clean(&view.spec.git_ref),
        clean(&view.spec.harness),
        view.spec.provider.as_deref().map(clean).unwrap_or_default(),
        view.spec.model.as_deref().map(clean).unwrap_or_default(),
        view.spec.timeout_secs,
        view.spec.idle_timeout_secs.unwrap_or_default(),
        after.join(","),
        waiting_on.join(","),
        artifacts.join(",")
    );
    Ok(ToolOutput {
        text,
        structured: json!({
            "id": clean(view.id.as_str()),
            "state": state,
            "attempt": view.attempt,
            "node": node,
            "epoch": view.epoch,
            "created_at": view.created_at,
            "started_at": view.started_at,
            "finished_at": view.finished_at,
            "failure": failure,
            "exit_code": body.get("exit_code").unwrap_or(&Value::Null),
            "spec": {
                "repo": clean(&view.spec.repo),
                "ref": clean(&view.spec.git_ref),
                "harness": clean(&view.spec.harness),
                "provider": view.spec.provider.as_deref().map(clean),
                "model": view.spec.model.as_deref().map(clean),
                "timeout_secs": view.spec.timeout_secs,
                "idle_timeout_secs": view.spec.idle_timeout_secs,
                "task": task
            },
            "summary": summary,
            "after": after,
            "waiting_on": waiting_on,
            "artifacts": artifacts
        }),
    })
}

async fn logs(host: &Host, args: &Value) -> Result<ToolOutput, ToolError> {
    let id = job_id(args)?;
    let lines = args["lines"].as_i64().unwrap_or(80) as usize;
    let page = connected(host)?.logs(&id, 0, None).await.map_err(failure)?;
    let stripped = strip_ansi(&page.data);
    let tail = stripped
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput {
        text: tail.clone(),
        structured: json!({
            "id": clean(id.as_str()),
            "lines": tail.lines().collect::<Vec<_>>(),
            "offset": page.offset,
            "terminal": page.terminal
        }),
    })
}

async fn nodes(host: &Host) -> Result<ToolOutput, ToolError> {
    let client = connected(host)?;
    let raw = client.nodes_raw().await.map_err(failure)?;
    let body: Value = serde_json::from_str(&raw).map_err(|error| {
        ToolError::Failed(one_line(&format!("bad controller node reply: {error}")))
    })?;
    // The controller answers with a bare array; older fixtures wrapped it in {"nodes": [...]}.
    let items: Vec<Value> = match &body {
        Value::Array(items) => items.clone(),
        other => other["nodes"].as_array().cloned().unwrap_or_default(),
    };
    let list: ListNodes = serde_json::from_value(json!({ "nodes": items })).map_err(|error| {
        ToolError::Failed(one_line(&format!("bad controller node reply: {error}")))
    })?;
    let now = unix_now();
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for node in &list.nodes {
        let cordons = items
            .iter()
            .find(|item| item["name"].as_str() == Some(node.name.as_str()))
            .and_then(|item| item["cordons"].as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(clean)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let state = if node.draining {
            "drained"
        } else if now.saturating_sub(node.last_seen) > STALE_AFTER_SECS {
            "stale"
        } else if node.harness == Some(HarnessState::Busy) {
            "busy"
        } else {
            "idle"
        };
        let providers = node
            .providers
            .iter()
            .map(|provider| {
                let state = match provider.state {
                    ProviderState::Ok => "ok",
                    ProviderState::Cooling => "cooling",
                    ProviderState::Disabled => "disabled",
                    ProviderState::Unknown => "unknown",
                };
                json!({
                    "name": clean(&provider.name),
                    "state": state,
                    "cooling_secs": provider.soonest_secs
                })
            })
            .collect::<Vec<_>>();
        let row = json!({
            "name": clean(node.name.as_str()),
            "state": state,
            "age_secs": now.saturating_sub(node.last_seen),
            "version": node.version.as_deref().map(clean),
            "providers": providers,
            "load1": node.load1,
            "mem_available_kb": node.mem_available_kb,
            "mem_total_kb": node.mem_total_kb,
            "stuck": node.stuck,
            "cordons": cordons
        });
        lines.push(format!(
            "{} {} age={}s version={} providers={} load1={} mem={}/{} stuck={} cordons={}",
            row["name"],
            row["state"],
            row["age_secs"],
            row["version"],
            row["providers"],
            row["load1"],
            row["mem_available_kb"],
            row["mem_total_kb"],
            row["stuck"],
            row["cordons"]
        ));
        rows.push(row);
    }
    Ok(ToolOutput {
        text: lines.join("\n"),
        structured: json!({"nodes": rows}),
    })
}

async fn graph(host: &Host) -> Result<ToolOutput, ToolError> {
    let page = connected(host)?
        .list_jobs(None, Some(200))
        .await
        .map_err(failure)?;
    let jobs = page
        .jobs
        .iter()
        .filter(|view| !view.state.is_terminal())
        .map(crate::operator::graph_job)
        .collect::<Vec<_>>();
    let graph = Graph::build(&jobs);
    let text = graph::render(&graph, 120)
        .iter()
        .map(|line| clean(&line.text()))
        .collect::<Vec<_>>()
        .join("\n");
    let nodes = graph
        .components
        .iter()
        .flat_map(|component| &component.nodes)
        .map(|node| {
            json!({
                "id": clean(&node.id),
                "state": clean(node.state.name()),
                "title": clean(&node.title),
                "after": node.after.iter().map(|id| clean(id)).collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    Ok(ToolOutput {
        text,
        structured: json!({"nodes": nodes}),
    })
}
