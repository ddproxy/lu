//! Tool table for `lu mcp`. No transport code lives here.

use lu_client::{Client, JobView, ListNodes};
use lu_proto::{truncate_chars, HarnessState, JobId, JobState, ProviderState};
use lu_tui::graph::{self, Graph};
use serde_json::{json, Map, Value};

use super::format::{cap_chars, cap_result, clean, first_line, one_line, strip_ansi, unix_now};
use super::protocol::{ToolError, ToolOutput};
use super::Host;

/// Two lease periods in the default controller configuration, used when `online` is absent.
const STALE_AFTER_SECS: u64 = 120;

struct Field {
    key: &'static str,
    kind: &'static str,
    description: &'static str,
    required: bool,
    default_int: Option<i64>,
    default_str: Option<&'static str>,
    min: Option<i64>,
    max: Option<i64>,
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
    }
}

const fn bounded(mut field: Field, min: i64, max: i64, default: i64) -> Field {
    field.min = Some(min);
    field.max = Some(max);
    field.default_int = Some(default);
    field
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    fields: &'static [Field],
}

const ID_FIELD: Field = required("id", "string", "Job id");
const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "lu_status",
        description: "Controller version, uptime, queue and node counts.",
        fields: &[],
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
    },
    ToolDef {
        name: "lu_job",
        description: "Full detail for one job, including its spec and artifacts.",
        fields: &[ID_FIELD],
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
    },
    ToolDef {
        name: "lu_nodes",
        description: "Node states, providers, load and cordons.",
        fields: &[],
    },
    ToolDef {
        name: "lu_graph",
        description: "Dependency graph of unfinished jobs.",
        fields: &[],
    },
];

pub fn list() -> Value {
    let tools = TOOLS
        .iter()
        .map(|tool| {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for field in tool.fields {
                let mut schema = Map::new();
                schema.insert("type".to_owned(), Value::String(field.kind.to_owned()));
                schema.insert(
                    "description".to_owned(),
                    Value::String(field.description.to_owned()),
                );
                if let Some(min) = field.min {
                    schema.insert("minimum".to_owned(), json!(min));
                }
                if let Some(max) = field.max {
                    schema.insert("maximum".to_owned(), json!(max));
                }
                if let Some(default) = field.default_int {
                    schema.insert("default".to_owned(), json!(default));
                }
                properties.insert(field.key.to_owned(), Value::Object(schema));
                if field.required {
                    required.push(Value::String(field.key.to_owned()));
                }
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
                "annotations": {"readOnlyHint": true}
            })
        })
        .collect::<Vec<_>>();
    Value::Array(tools)
}

fn validate(def: &ToolDef, args: &Value) -> Result<Value, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| ToolError::Args("tool arguments must be an object".to_owned()))?;
    for key in object.keys() {
        if !def.fields.iter().any(|field| field.key == key) {
            return Err(ToolError::Args(format!("unknown argument: {}", clean(key))));
        }
    }
    let mut out = Map::new();
    for field in def.fields {
        match object.get(field.key) {
            Some(value) => match field.kind {
                "string" => {
                    let text = value.as_str().ok_or_else(|| {
                        ToolError::Args(format!("{} must be a string", field.key))
                    })?;
                    out.insert(field.key.to_owned(), Value::String(clean(text)));
                }
                "integer" => {
                    let number = value.as_i64().ok_or_else(|| {
                        ToolError::Args(format!("{} must be an integer", field.key))
                    })?;
                    if let Some(min) = field.min {
                        if number < min {
                            return Err(ToolError::Args(format!(
                                "{} must be at least {min}",
                                field.key
                            )));
                        }
                    }
                    if let Some(max) = field.max {
                        if number > max {
                            return Err(ToolError::Args(format!(
                                "{} must be at most {max}",
                                field.key
                            )));
                        }
                    }
                    out.insert(field.key.to_owned(), json!(number));
                }
                _ => unreachable!("tool fields are declared as string or integer"),
            },
            None if field.required => {
                return Err(ToolError::Args(format!("missing argument: {}", field.key)));
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

pub async fn call(host: &Host, name: &str, args: &Value) -> Result<ToolOutput, ToolError> {
    let def = TOOLS
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| ToolError::Args(format!("unknown tool: {}", clean(name))))?;
    let args = validate(def, args)?;
    let output = match name {
        "lu_status" => status(host).await?,
        "lu_jobs" => jobs(host, &args).await?,
        "lu_job" => job(host, &args).await?,
        "lu_logs" => logs(host, &args).await?,
        "lu_nodes" => nodes(host).await?,
        "lu_graph" => graph(host).await?,
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
