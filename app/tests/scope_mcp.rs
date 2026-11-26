//! Binary-level tests for the scoped MCP server in `ting`.

mod common;

use common::{run_bin, scratch, stderr, stdout};
use serde_json::{json, Value};
use std::path::Path;

const TOOLS: [&str; 6] = [
    "job_context",
    "submit_child",
    "list_children",
    "get_child",
    "get_child_artifact",
    "cancel_child",
];

fn mcp_env(dir: &Path) -> Vec<(String, String)> {
    vec![
        (
            "LU_JOB_SOCKET".to_owned(),
            dir.join("job.sock").display().to_string(),
        ),
        (
            "LU_JOB_SCOPE_CAPABILITY".to_owned(),
            "fixture-capability".to_owned(),
        ),
        (
            "XDG_CONFIG_HOME".to_owned(),
            dir.join("config").display().to_string(),
        ),
        ("HOME".to_owned(), dir.display().to_string()),
    ]
}

fn run_mcp(bin: &str, dir: &Path, envs: &[(String, String)], input: &str) -> std::process::Output {
    let borrowed: Vec<(&str, &str)> = envs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    run_bin(bin, &["mcp"], &borrowed, Some(input), dir)
}

fn mcp_input() -> String {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}
    });
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    format!("{initialize}\n{list}\n")
}

fn replies(output: &str) -> Vec<Value> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn ting_mcp_exposes_exactly_the_six_scoped_tools() {
    let dir = scratch("ting-mcp-tools");
    let envs = mcp_env(&dir);
    let output = run_mcp("ting", &dir, &envs, &mcp_input());
    assert!(output.status.success(), "{}", stderr(&output));
    let replies = replies(&stdout(&output));
    let init = replies
        .iter()
        .find(|value| value["id"] == 1)
        .expect("initialize reply");
    assert_eq!(init["result"]["serverInfo"]["name"], "ting");
    assert_eq!(
        init["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    let list = replies
        .iter()
        .find(|value| value["id"] == 2)
        .expect("tools/list reply");
    let tools = list["result"]["tools"].as_array().expect("tool array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("<missing>"))
        .collect();
    assert_eq!(names, TOOLS);
    // Scoped tools are confined to this job. Drafts and other jobs are operator-only.
    assert!(!names.iter().any(|name| name.contains("draft")));
    for tool in tools {
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(description.contains("untrusted"), "{description}");
        assert!(description.contains("child_job"), "{description}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scoped_server_refuses_without_a_socket_capability() {
    let dir = scratch("ting-mcp-no-capability");
    let mut envs = mcp_env(&dir);
    envs.retain(|(key, _)| key != "LU_JOB_SCOPE_CAPABILITY");
    let output = run_mcp("ting", &dir, &envs, &mcp_input());
    assert!(
        !output.status.success(),
        "ting started without a capability"
    );
    assert!(
        stderr(&output).contains("SCOPE_CAPABILITY"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scoped_server_refuses_when_operator_credentials_are_visible() {
    let dir = scratch("ting-mcp-refusal");
    let mut envs = mcp_env(&dir);

    envs.push(("LU_TOKEN".to_owned(), "operator-secret".to_owned()));
    let output = run_mcp("ting", &dir, &envs, &mcp_input());
    assert!(!output.status.success(), "ting started with LU_TOKEN");
    assert!(
        stderr(&output).contains("refusing to start"),
        "{}",
        stderr(&output)
    );

    envs.pop();
    let config = dir.join("config").join("lu");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("operator.toml"), "[operator]\n").unwrap();
    let output = run_mcp("ting", &dir, &envs, &mcp_input());
    assert!(!output.status.success(), "ting started with a config file");
    assert!(
        stderr(&output).contains("operator config"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
