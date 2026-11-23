//! Binary-level tests for `ting`'s self-service commands and their exit codes.

mod common;

use std::path::{Path, PathBuf};

use common::{run_bin, scratch, write_token, Reply};
use serde_json::{json, Value};

fn write_config(dir: &Path, url: &str, token_file: &Path) -> PathBuf {
    let path = dir.join("ting.toml");
    let work = dir.join("work");
    let text = format!(
        "[agent]\nname = \"node-a\"\ncontroller_url = \"{url}\"\ntoken_file = \"{}\"\nwork_dir = \"{}\"\nharness = \"opencode\"\n",
        token_file.display(),
        work.display()
    );
    std::fs::write(&path, text).unwrap();
    path
}

/// Tokens are generated at runtime so no credential-shaped literal appears in the source.
fn setup(label: &str) -> (PathBuf, PathBuf, String) {
    let dir = scratch(label);
    let token = lu_proto::JobId::new().to_string();
    let token_file = write_token(&dir, "node.token", &token);
    (dir, token_file, token)
}

fn self_json(draining: bool, job: Option<&str>) -> Value {
    json!({
        "name": "node-a",
        "version": "9.9.9",
        "draining": draining,
        "current_job": job,
        "last_heartbeat_age_secs": 2,
        "online": true
    })
}

fn ok_json(value: &Value) -> Reply {
    Reply {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: value.to_string().into_bytes(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_json_is_stable_and_never_prints_the_token() {
    let server = common::start(|_| ok_json(&self_json(true, None))).await;
    let (dir, token_file, token) = setup("ting-self-status");
    let config = write_config(&dir, &server.url, &token_file);

    let out = run_bin(
        "ting",
        &["status", "--json", "--config", config.to_str().unwrap()],
        &[],
        None,
        &dir,
    );
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(
        common::stdout(&out).trim(),
        r#"{"name":"node-a","version":"9.9.9","draining":true,"current_job":null,"last_heartbeat_age_secs":2,"online":true}"#
    );
    assert!(!common::stdout(&out).contains(&token));
    assert!(!common::stderr(&out).contains(&token));
    let records = server.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[0].path, "/v1/agent/self");
    assert_eq!(
        records[0].header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_and_undrain_use_the_self_routes() {
    let server = common::start(|_| ok_json(&json!({}))).await;
    let (dir, token_file, token) = setup("ting-self-drain");
    let config = write_config(&dir, &server.url, &token_file);
    let config = config.to_str().unwrap();

    for command in ["drain", "undrain"] {
        let out = run_bin("ting", &[command, "--config", config], &[], None, &dir);
        assert!(out.status.success(), "{command} {:?}", out);
        assert!(common::stdout(&out).is_empty());
        assert!(!common::stderr(&out).contains(&token));
    }
    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, vec!["/v1/agent/drain", "/v1/agent/undrain"]);
    for record in records.iter() {
        assert_eq!(record.method, "POST");
        assert_eq!(record.body, b"{}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_idle_exits_zero_when_drained_and_idle() {
    let server = common::start(|_| ok_json(&self_json(true, None))).await;
    let (dir, token_file, token) = setup("ting-self-idle");
    let config = write_config(&dir, &server.url, &token_file);

    let out = run_bin(
        "ting",
        &[
            "wait-idle",
            "--timeout",
            "5s",
            "--config",
            config.to_str().unwrap(),
        ],
        &[],
        None,
        &dir,
    );
    assert!(out.status.success(), "{:?}", out);
    assert!(!common::stdout(&out).contains(&token));
    assert_eq!(server.records.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_idle_exits_three_on_timeout() {
    let server =
        common::start(|_| ok_json(&self_json(true, Some("01ARZ3NDEKTSV4RRFFQ69G5FAV")))).await;
    let (dir, token_file, token) = setup("ting-self-timeout");
    let config = write_config(&dir, &server.url, &token_file);

    let out = run_bin(
        "ting",
        &[
            "wait-idle",
            "--timeout",
            "1",
            "--config",
            config.to_str().unwrap(),
        ],
        &[],
        None,
        &dir,
    );
    assert_eq!(out.status.code(), Some(3), "{:?}", out);
    assert!(common::stderr(&out).contains("timed out"), "{:?}", out);
    assert!(!common::stderr(&out).contains(&token));
    assert!(server.records.lock().unwrap().len() >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_and_usage_errors_exit_two() {
    let dir = scratch("ting-self-errors");
    let missing = dir.join("missing.toml");
    let out = run_bin(
        "ting",
        &["status", "--json", "--config", missing.to_str().unwrap()],
        &[],
        None,
        &dir,
    );
    assert_eq!(out.status.code(), Some(2), "{:?}", out);
    assert!(
        common::stderr(&out).contains("cannot read config"),
        "{:?}",
        out
    );

    let empty = dir.join("empty.toml");
    std::fs::write(&empty, "[log]\nlevel = \"info\"\n").unwrap();
    let out = run_bin(
        "ting",
        &["status", "--config", empty.to_str().unwrap()],
        &[],
        None,
        &dir,
    );
    assert_eq!(out.status.code(), Some(2), "{:?}", out);
    assert!(
        common::stderr(&out).contains("at least one of [controller] and [agent]"),
        "{:?}",
        out
    );

    let out = run_bin("ting", &["wait-idle", "--timeout", "0"], &[], None, &dir);
    assert_eq!(out.status.code(), Some(2), "{:?}", out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_errors_exit_one_and_keep_the_token_hidden() {
    let server = common::start(|_| Reply {
        status: 401,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: br#"{"error":"unauthorized","message":"unauthorized"}"#.to_vec(),
    })
    .await;
    let (dir, token_file, token) = setup("ting-self-error");
    let config = write_config(&dir, &server.url, &token_file);

    let out = run_bin(
        "ting",
        &["status", "--json", "--config", config.to_str().unwrap()],
        &[],
        None,
        &dir,
    );
    assert_eq!(out.status.code(), Some(1), "{:?}", out);
    assert!(common::stderr(&out).contains("unauthorized"), "{:?}", out);
    assert!(!common::stdout(&out).contains(&token));
    assert!(!common::stderr(&out).contains(&token));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn help_lists_the_self_service_commands() {
    let dir = scratch("ting-self-help");
    let out = run_bin("ting", &["--help"], &[], None, &dir);
    assert!(out.status.success(), "{:?}", out);
    let help = common::stdout(&out);
    for command in ["drain", "undrain", "status", "wait-idle"] {
        assert!(help.contains(command), "{help}");
    }
    assert!(help.contains("/etc/ting/ting.toml"), "{help}");
}
