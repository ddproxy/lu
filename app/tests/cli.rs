//! End-to-end CLI tests against an in-process fake controller. No real controller is needed.

mod common;

use common::*;
use lu_proto::ArtifactName;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_json_prints_the_raw_body_and_sends_the_filters() {
    let id = job_id();
    let body = json!({"jobs": [job_json(&id, "running")], "future": true});
    let expected = body.to_string();
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("ls-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["--json", "ls", "--state", "running", "--limit", "2"],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[0].path, "/v1/jobs");
    assert_eq!(records[0].query.as_deref(), Some("state=running&limit=2"));
    let authorization = format!("Bearer {TOKEN}");
    assert_eq!(
        records[0].header("authorization"),
        Some(authorization.as_str())
    );
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "tui")]
async fn top_once_prints_tables_and_top_json_prints_one_object() {
    let id = job_id();
    let node_body = json!([{
        "name": "node-a",
        "harness": "idle",
        "providers": [],
        "last_seen": 20,
        "draining": false
    }]);
    let jobs_body = json!({"jobs": [job_json(&id, "running")]});
    let server = start(move |record| {
        if record.path.contains("nodes") {
            Reply::json(&node_body)
        } else if record.path.contains("status") {
            Reply::json(&json!({}))
        } else {
            Reply::json(&jobs_body)
        }
    })
    .await;
    let dir = scratch("top-once-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["top", "--once"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("NODE"), "{text}");
    assert!(text.contains("node-a"), "{text}");
    assert!(text.contains(&id[id.len() - 10..]), "{text}");
    assert!(!text.contains("lu top"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");

    let output = run_with_env(&["top", "--json"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(value["nodes"][0]["name"], "node-a");
    assert_eq!(value["jobs"][0]["id"], id);

    let output = run_with_env(&["top", "--once", "--json"], &envs, None, &dir);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("mutually exclusive"),
        "{}",
        stderr(&output)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_tree_shows_scope_lineage() {
    let parent_id = job_id();
    let child_id = job_id();
    let mut parent = job_json(&parent_id, "running");
    parent["spec"]["task"] = json!("parent task");
    parent["root_job_id"] = json!(parent_id);
    parent["depth"] = json!(0);
    let mut child = job_json(&child_id, "queued");
    child["spec"]["task"] = json!("child task");
    child["parent_job_id"] = json!(parent_id);
    child["root_job_id"] = json!(parent_id);
    child["depth"] = json!(1);
    let body = json!({"jobs": [parent, child]});
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("ls-tree");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["ls", "--tree"], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("{parent_id}  running   parent task")),
        "{text}"
    );
    assert!(text.contains(&format!("└─ {child_id}")), "{text}");
    assert!(text.contains("child task"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_reports_scope_lineage_and_spawn_settings() {
    let parent_id = job_id();
    let child_id = job_id();
    let mut job = job_json(&child_id, "running");
    job["parent_job_id"] = json!(parent_id);
    job["root_job_id"] = json!(parent_id);
    job["depth"] = json!(1);
    job["spec"]["allow_spawn"] = json!(true);
    job["spec"]["spawn_inherit"] = json!(false);
    job["spec"]["spawn_repos"] = json!(["https://example.net/allowed.git"]);
    let server = start(move |_| Reply::json(&job)).await;
    let dir = scratch("show-scope");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["show", &child_id], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for expected in [
        format!("parent              {parent_id}"),
        format!("root                {parent_id}"),
        "depth               1".to_owned(),
        "allow_spawn         true".to_owned(),
        "spawn_inherit       false".to_owned(),
        "https://example.net/allowed.git".to_owned(),
    ] {
        assert!(text.contains(&expected), "missing {expected:?} in {text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "tui")]
async fn top_marks_scoped_children() {
    let parent_id = job_id();
    let child_id = job_id();
    let mut parent = job_json(&parent_id, "running");
    parent["root_job_id"] = json!(parent_id);
    parent["depth"] = json!(0);
    let mut child = job_json(&child_id, "running");
    child["parent_job_id"] = json!(parent_id);
    child["root_job_id"] = json!(parent_id);
    child["depth"] = json!(1);
    let node_body = json!([{
        "name": "node-a",
        "harness": "idle",
        "providers": [],
        "last_seen": 20,
        "draining": false
    }]);
    let jobs_body = json!({"jobs": [parent, child]});
    let server = start(move |record| {
        if record.path.contains("nodes") {
            Reply::json(&node_body)
        } else if record.path.contains("status") {
            Reply::json(&json!({}))
        } else {
            Reply::json(&jobs_body)
        }
    })
    .await;
    let dir = scratch("top-scope");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["top", "--once"], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("scope:d1"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_uses_the_operator_file_and_sanitises_hostile_text() {
    let id = job_id();
    let mut job = job_json(&id, "failed");
    job["node"] = json!("node-a");
    job["assigned_provider"] = json!("peer-b");
    job["failure_class"] = json!("execution");
    job["summary"] = json!("bad tests\n\u{1b}[31m");
    job["spec"]["fallbacks"] = json!([{"provider": "peer-b", "model": "m2"}]);
    let server = start(move |_| Reply::json(&job)).await;
    let dir = scratch("show-config");
    let token = write_token(&dir, "operator.token", TOKEN);
    let xdg = dir.join("xdg");
    std::fs::create_dir_all(xdg.join("lu")).unwrap();
    std::fs::write(
        xdg.join("lu").join("operator.toml"),
        format!(
            "url = \"{}\"\ntoken_file = \"{}\"\n",
            server.url,
            token.display()
        ),
    )
    .unwrap();
    // An [agent] table in --config must never supply operator credentials.
    let config = dir.join("lu.toml");
    std::fs::write(
        &config,
        format!(
            r#"[agent]
name = "agent"
controller_url = "http://127.0.0.1:1"
token_file = "{}"
work_dir = "/srv/work/lu"
harness = "opencode"
"#,
            token.display()
        ),
    )
    .unwrap();

    let output = run(
        &["--config", config.to_str().unwrap(), "show", &id],
        &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())],
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("bad tests??[31m"), "{text}");
    assert!(text.contains("node-a"), "{text}");
    assert!(text.contains("execution"), "{text}");
    assert!(text.contains("peer-b"), "{text}");
    assert!(text.contains("peer-b:m2"), "{text}");
    let state = text
        .lines()
        .find(|line| line.trim_start().starts_with("state"))
        .unwrap();
    assert!(state.ends_with("failed"), "{state:?}");
    assert_eq!(
        server.records.lock().unwrap()[0].path,
        format!("/v1/jobs/{id}")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_passes_after_dependencies() {
    let id = job_id();
    let dep1 = job_id();
    let dep2 = job_id();
    let body = job_json(&id, "blocked");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-after");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--after",
            &dep1,
            "--after",
            &dep2,
        ],
        &env_for(&server, &token),
        Some("run after the others\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[0].body).unwrap();
    assert_eq!(spec["after"], json!([dep1, dep2]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_passes_required_labels_and_prints_warnings() {
    let id = job_id();
    let mut body = job_json(&id, "queued");
    body["warnings"] = json!(["no node with label php8.3"]);
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-require");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--task",
            "go",
            "--require",
            "php8.3",
            "--require",
            "gpu",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), id);
    assert!(
        stderr(&output).contains("lu: warning: no node with label php8.3"),
        "{}",
        stderr(&output)
    );
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[0].body).unwrap();
    assert_eq!(spec["requires"], json!(["php8.3", "gpu"]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_passes_tolerations() {
    let id = job_id();
    let mut body = job_json(&id, "queued");
    body["warnings"] = json!([]);
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-tolerate");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--task",
            "go",
            "--tolerate",
            "homelab",
            "--tolerate",
            "garage",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), id);
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[0].body).unwrap();
    assert_eq!(spec["tolerates"], json!(["homelab", "garage"]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_after_key_chains_three_jobs() {
    let build = job_id();
    let test = job_id();
    let ship = job_id();
    let keys = [
        ("build".to_owned(), build.clone()),
        ("test".to_owned(), test.clone()),
        ("ship".to_owned(), ship.clone()),
    ];
    let server = start(move |record| {
        if record.method == "GET" {
            let key = record
                .query
                .as_deref()
                .and_then(|q| q.strip_prefix("key="))
                .unwrap_or("");
            let jobs: Vec<Value> = keys
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, id)| job_json(id, "done"))
                .collect();
            return Reply::json(&json!({"jobs": jobs, "next": null}));
        }
        let spec: Value = serde_json::from_slice(&record.body).unwrap();
        let key = spec["idempotency_key"].as_str().unwrap_or("");
        let id = keys
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, id)| id.clone())
            .unwrap_or_else(job_id);
        Reply::json(&job_json(&id, "blocked"))
    })
    .await;
    let dir = scratch("submit-after-key-chain");
    let token = write_token(&dir, "operator.token", TOKEN);
    let env = env_for(&server, &token);
    for args in [
        vec![
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--key",
            "build",
            "--task",
            "build",
        ],
        vec![
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--key",
            "test",
            "--after-key",
            "build",
            "--task",
            "test",
        ],
        vec![
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--key",
            "ship",
            "--after-key",
            "build",
            "--after-key",
            "test",
            "--task",
            "ship",
        ],
    ] {
        let output = run_with_env(&args, &env, None, &dir);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let records = server.records.lock().unwrap();
    let submitted: Vec<Value> = records
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(submitted.len(), 3);
    assert!(submitted[0].get("after").is_none(), "{}", submitted[0]);
    assert_eq!(submitted[1]["after"], json!([build]));
    assert_eq!(submitted[2]["after"], json!([build, test]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_after_key_unknown_key_is_a_usage_error() {
    let server = start(|_| Reply::json(&json!({"jobs": [], "next": null}))).await;
    let dir = scratch("submit-after-key-unknown");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--task",
            "go",
            "--after-key",
            "no-such-key",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no-such-key"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(server
        .records
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_after_and_after_key_resolve_to_one_dependency() {
    let dep = job_id();
    let new_id = job_id();
    let dep_id = dep.clone();
    let server = start(move |record| {
        if record.method == "GET" {
            return Reply::json(&json!({"jobs": [job_json(&dep_id, "done")], "next": null}));
        }
        Reply::json(&job_json(&new_id, "blocked"))
    })
    .await;
    let dir = scratch("submit-after-dedup");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--task",
            "go",
            "--after",
            &dep,
            "--after-key",
            "build",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let records = server.records.lock().unwrap();
    let post = records.iter().find(|r| r.method == "POST").unwrap();
    let spec: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(spec["after"], json!([dep]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_allow_empty_sets_and_default_omits_the_field() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-allow-empty");
    let token = write_token(&dir, "operator.token", TOKEN);

    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--allow-empty",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[0].body).unwrap();
    assert_eq!(spec["allow_empty"], json!(true));

    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[1].body).unwrap();
    assert!(
        spec.get("allow_empty").is_none(),
        "the default must be omitted: {spec}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_prints_the_no_changes_note_column() {
    let noted_id = job_id();
    let plain_id = job_id();
    let mut noted = job_json(&noted_id, "done");
    noted["outcome_note"] = json!("no_changes");
    let plain = job_json(&plain_id, "done");
    let body = json!({"jobs": [noted, plain]});
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("ls-note");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["ls"], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let header: Vec<&str> = text.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header[6], "NOTE", "{text}");
    let noted_line = text.lines().find(|line| line.contains(&noted_id)).unwrap();
    assert!(noted_line.trim_end().ends_with("no changes"), "{text}");
    let plain_line = text.lines().find(|line| line.contains(&plain_id)).unwrap();
    assert!(plain_line.trim_end().ends_with('-'), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_json_keeps_the_outcome_note_field() {
    let id = job_id();
    let mut job = job_json(&id, "done");
    job["outcome_note"] = json!("no_changes");
    let body = json!({"jobs": [job]});
    let expected = body.to_string();
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("ls-note-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["--json", "ls"], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    assert!(stdout(&output).contains("\"outcome_note\":\"no_changes\""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_prints_the_no_changes_outcome_note() {
    let id = job_id();
    let main_id = id.clone();
    let server = start(move |_| {
        let mut view = job_json(&main_id, "done");
        view["outcome_note"] = json!("no_changes");
        view["scrubbed_hits"] = json!(4);
        view["patch_scrubbed"] = json!(true);
        Reply::json(&view)
    })
    .await;
    let dir = scratch("show-note");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["show", &id], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("outcome_note")
                && line.trim_end().ends_with("no changes")),
        "{text}"
    );
    let field = |name: &str| {
        text.lines()
            .find_map(|line| {
                let mut cells = line.split_whitespace();
                (cells.next() == Some(name)).then(|| cells.next().unwrap_or(""))
            })
            .unwrap_or("")
    };
    assert_eq!(field("scrubbed_hits"), "4", "{text}");
    assert_eq!(field("patch_scrubbed"), "true", "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_shows_the_waiting_dependency_count() {
    let waiting_id = job_id();
    let free_id = job_id();
    let dep = job_id();
    let mut waiting = job_json(&waiting_id, "blocked");
    waiting["after"] = json!([dep]);
    waiting["waiting_on"] = json!([dep]);
    let free = job_json(&free_id, "queued");
    let body = json!({"jobs": [waiting, free]});
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("ls-after");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["ls"], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let header = text.lines().next().unwrap();
    let columns: Vec<&str> = header.split_whitespace().collect();
    assert_eq!(columns[4], "AFTER", "{text}");
    let waiting_line = text
        .lines()
        .find(|line| line.contains(&waiting_id))
        .unwrap();
    let cells: Vec<&str> = waiting_line.split_whitespace().collect();
    assert_eq!(cells[1], "blocked");
    assert_eq!(cells[4], "1");
    let free_line = text.lines().find(|line| line.contains(&free_id)).unwrap();
    let cells: Vec<&str> = free_line.split_whitespace().collect();
    assert_eq!(cells[4], "-");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_lists_dependency_states() {
    let id = job_id();
    let dep = job_id();
    let main_id = id.clone();
    let dep_id = dep.clone();
    let server = start(move |recorded| {
        if recorded.path == format!("/v1/jobs/{dep_id}") {
            Reply::json(&job_json(&dep_id, "blocked"))
        } else {
            let mut view = job_json(&main_id, "blocked");
            view["after"] = json!([dep_id]);
            view["waiting_on"] = json!([dep_id]);
            Reply::json(&view)
        }
    })
    .await;
    let dir = scratch("show-after");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["show", &id], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("after  {dep}  blocked (waiting)")),
        "{text}"
    );
    assert!(
        server
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|record| record.path == format!("/v1/jobs/{dep}")),
        "the dependency should be fetched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_draws_a_diamond_and_prints_its_nodes_as_json() {
    let a = job_id();
    let b = job_id();
    let c = job_id();
    let d = job_id();
    let mut joined = job_json(&d, "blocked");
    joined["after"] = json!([b, c]);
    let mut right = job_json(&c, "failed");
    right["after"] = json!([a]);
    let mut left = job_json(&b, "running");
    left["after"] = json!([a]);
    let root = job_json(&a, "done");
    let body = json!({"jobs": [joined, right, left, root]});
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("graph-diamond");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["graph"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains('✓'), "{text}");
    assert!(text.contains('▶'), "{text}");
    assert!(text.contains('✗'), "{text}");
    assert!(text.contains('◌'), "{text}");
    assert!(text.contains("┐") && text.contains("└"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");

    let output = run_with_env(&["--json", "graph"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    let nodes = value["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 4);
    assert!(nodes
        .iter()
        .any(|node| node["id"] == a && node["state"] == "done"));
    assert!(nodes
        .iter()
        .any(|node| node["id"] == d && node["state"] == "blocked"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_reads_stdin_and_sends_every_flag() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-stdin");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--harness",
            "opencode",
            "--provider",
            "peer-a",
            "--model",
            "big-pickle",
            "--timeout",
            "90",
            "--key",
            "ticket:42",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), id);
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].path, "/v1/jobs");
    assert_eq!(records[0].header("idempotency-key"), Some("ticket:42"));
    let spec: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(spec["source"]["repo"], "https://example.net/repo.git");
    assert_eq!(spec["source"]["ref"], "main");
    assert_eq!(spec["task"], "run the tests\n");
    assert_eq!(spec["provider"], "peer-a");
    assert_eq!(spec["model"], "big-pickle");
    assert_eq!(spec["timeout_secs"], 90);
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_passes_fallback_choices() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-fallback");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--provider",
            "peer-a",
            "--model",
            "primary-model",
            "--fallback",
            "peer-b",
            "--fallback",
            "peer-c:model-c:tag",
        ],
        &env_for(&server, &token),
        Some("keep going\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap()[0].body).unwrap();
    assert_eq!(spec["provider"], "peer-a");
    assert_eq!(spec["model"], "primary-model");
    assert_eq!(
        spec["fallbacks"],
        json!([
            {"provider": "peer-b"},
            {"provider": "peer-c", "model": "model-c:tag"},
        ])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_sends_idle_timeout_and_rejects_zero() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-idle");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--idle-timeout",
            "45",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    {
        let records = server.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        let spec: Value = serde_json::from_slice(&records[0].body).unwrap();
        assert_eq!(spec["idle_timeout_secs"], 45);
    }

    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--idle-timeout",
            "0",
        ],
        &env_for(&server, &token),
        Some("run the tests\n"),
        &dir,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(
        server.records.lock().unwrap().len(),
        1,
        "a usage error must not reach the controller"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_reads_a_task_file() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-file");
    let token = write_token(&dir, "operator.token", TOKEN);
    let task = dir.join("task.txt");
    std::fs::write(&task, "from the file").unwrap();
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "/srv/repo",
            "--ref",
            "main",
            "--task-file",
            task.to_str().unwrap(),
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let records = server.records.lock().unwrap();
    let spec: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(spec["task"], "from the file");
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_wait_exits_three_when_the_job_fails() {
    let id = job_id();
    let post_id = id.clone();
    let get_id = id.clone();
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&job_json(&post_id, "queued"))
        } else {
            Reply::json(&job_json(&get_id, "failed"))
        }
    })
    .await;
    let dir = scratch("submit-wait");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "submit",
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--task",
            "go",
            "--wait",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), id);
    assert!(
        stderr(&output).contains("state: failed"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_follow_sanitises_escapes_and_exits_three_on_failure() {
    let id = job_id();
    let log_id = id.clone();
    let calls = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&calls);
    let server = start(move |record| {
        assert!(record.path.ends_with("/logs"));
        let mut count = counter.lock().unwrap();
        *count += 1;
        if *count == 1 {
            assert_eq!(record.query.as_deref(), Some("offset=0&wait=25"));
            Reply::json(&json!({"data": "ok\u{1b}[31m\n", "next": 5, "state": "running"}))
        } else {
            assert_eq!(record.query.as_deref(), Some("offset=5&wait=25"));
            Reply::json(&json!({
                "data": format!("bad {}", log_id),
                "next": 9,
                "state": "failed",
                "terminal": true
            }))
        }
    })
    .await;
    let dir = scratch("logs-follow");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["logs", &id, "--follow"],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let text = stdout(&output);
    assert_eq!(text, format!("ok?[31m\nbad {id}"));
    assert!(!text.contains('\u{1b}'));
    assert_eq!(*calls.lock().unwrap(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_writes_the_fixed_private_names_and_needs_force_to_replace() {
    let id = job_id();
    let server = start(|record| {
        if record.path.ends_with(ArtifactName::DiffPatch.file_name()) {
            Reply::ok(b"diff --git".to_vec())
        } else if record.path.ends_with(ArtifactName::RunLog.file_name()) {
            Reply::ok(b"log line\n".to_vec())
        } else {
            Reply::ok(br#"{"ok":true}"#.to_vec())
        }
    })
    .await;
    let dir = scratch("fetch");
    let token = write_token(&dir, "operator.token", TOKEN);
    let out = dir.join("artifacts");
    let envs = env_for(&server, &token);

    let output = run_with_env(
        &["fetch", &id, "--out", out.to_str().unwrap()],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    for name in ArtifactName::ALL {
        let path = out.join(name.file_name());
        assert!(path.is_file(), "missing {}", path.display());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{} has mode {mode:o}", path.display());
    }
    assert_eq!(
        std::fs::read(out.join("diff.patch")).unwrap(),
        b"diff --git"
    );

    let output = run_with_env(
        &["fetch", &id, "--out", out.to_str().unwrap()],
        &envs,
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );

    let output = run_with_env(
        &["fetch", &id, "--out", out.to_str().unwrap(), "--force"],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let records = server.records.lock().unwrap();
    assert!(records.iter().all(|record| record
        .path
        .starts_with(&format!("/v1/jobs/{id}/artifacts/"))));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_job_with_artifacts_can_be_fetched_and_shows_its_diff_size() {
    let id = job_id();
    let mut job = job_json(&id, "failed");
    job["failure_class"] = json!("execution");
    job["summary"] = json!("the prompt could not be answered");
    job["diff_bytes"] = json!(1024);
    job["artifacts"] = json!(["diff.patch", "run.log"]);
    let job_body = job.clone();
    let job_path = format!("/v1/jobs/{id}");
    let server = start(move |record| {
        if record.path == job_path {
            Reply::json(&job_body)
        } else if record.path.ends_with(ArtifactName::DiffPatch.file_name()) {
            Reply::ok(b"diff --git a/a b/a\n+partial work\n".to_vec())
        } else if record.path.ends_with(ArtifactName::RunLog.file_name()) {
            Reply::ok(b"log line\n".to_vec())
        } else {
            Reply::status(404)
        }
    })
    .await;
    let dir = scratch("fetch-failed");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    // `show` displays the size of the partial diff, and `--json ls` forwards the
    // controller's field verbatim.
    let output = run_with_env(&["show", &id], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("failed"), "{text}");
    assert!(text.contains("1024"), "{text}");
    assert!(text.contains("the prompt could not be answered"), "{text}");

    let list = json!({"jobs": [job], "future": true});
    let json_server = start(move |_| Reply::json(&list)).await;
    let json_envs = env_for(&json_server, &token);
    let output = run_with_env(&["--json", "ls"], &json_envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(value["jobs"][0]["diff_bytes"], json!(1024), "{value}");

    // A failed job is not a barrier to fetching whatever artifacts it did write.
    let out = dir.join("artifacts");
    let output = run_with_env(
        &["fetch", &id, "--out", out.to_str().unwrap()],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        std::fs::read(out.join("diff.patch")).unwrap(),
        b"diff --git a/a b/a\n+partial work\n"
    );
    assert_eq!(std::fs::read(out.join("run.log")).unwrap(), b"log line\n");
    assert!(!out.join("report.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_and_drain_render_aligned_text() {
    let list = json!([
        {
            "name": "node-a",
            "version": "0.1.10",
            "harness": "idle",
            "last_seen": 10,
            "draining": true,
            "labels": ["gpu", "php8.3", "ubuntu22.04", "x86_64", "region=eu", "tier=gold"],
            "taints": ["homelab"],
            "cordon_details": [
                {"provider": "zen", "source": "failure", "reason": "cooldown", "set_at": 0, "until": 20, "lifts_after_healthy_heartbeats": null},
                {"provider": "openrouter", "source": "failure", "reason": "cooldown", "set_at": 0, "until": 20, "lifts_after_healthy_heartbeats": null}
            ]
        },
        {"name": "node-b", "harness": "busy", "last_seen": 12, "draining": false, "labels": []}
    ]);
    let server = start(move |record| {
        if record.path == "/v1/nodes" {
            Reply::json(&list)
        } else if record.path.ends_with("/drain") {
            Reply::status(204)
        } else {
            Reply::json(&json!({"name": "node-a", "draining": false}))
        }
    })
    .await;
    let dir = scratch("nodes");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["nodes"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let header = text.lines().next().unwrap();
    assert!(header.starts_with("NAME"), "{text}");
    assert!(header.contains("VERSION"), "{text}");
    assert!(header.contains("HARNESS"), "{text}");
    assert!(header.contains("LABELS"), "{text}");
    assert!(header.contains("CORDONS"), "{text}");
    assert!(text.contains("node-a"), "{text}");
    assert!(text.contains("0.1.10"), "{text}");
    assert!(
        text.contains("!homelab,gpu,php8.3,ubuntu22.04,x86_64+2"),
        "{text}"
    );
    assert!(text.contains("!zen,!openrouter"), "{text}");
    assert!(text.contains("node-b"), "{text}");
    assert!(text.contains("yes"), "{text}");

    let output = run_with_env(&["drain", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let drain_line = stdout(&output)
        .lines()
        .find(|line| line.starts_with("draining"))
        .unwrap()
        .to_owned();
    assert!(drain_line.ends_with("yes"), "{drain_line:?}");

    let output = run_with_env(&["undrain", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let undrain_line = stdout(&output)
        .lines()
        .find(|line| line.starts_with("draining"))
        .unwrap()
        .to_owned();
    assert!(undrain_line.ends_with("no"), "{undrain_line:?}");

    let output = run_with_env(&["--json", "nodes"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let raw = stdout(&output);
    assert!(raw.contains("\"labels\":[\"gpu\""), "{raw}");
    assert!(raw.contains("\"taints\":[\"homelab\"]"), "{raw}");

    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
    assert_eq!(paths[0], "/v1/nodes");
    assert_eq!(paths[1], "/v1/nodes/node-a/drain");
    assert_eq!(paths[2], "/v1/nodes/node-a/undrain");
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncordon_sends_the_optional_provider_and_prints_what_was_removed() {
    let server = start(move |record| {
        if record.path == "/v1/nodes/node-a/uncordon" {
            let value: Value = if record.body.is_empty() {
                json!({"removed": ["zen", "openrouter"]})
            } else {
                let request: Value = serde_json::from_slice(&record.body).unwrap();
                json!({"removed": [request["provider"].as_str().unwrap()]})
            };
            Reply::json(&value)
        } else {
            Reply::status(404)
        }
    })
    .await;
    let dir = scratch("uncordon");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["uncordon", "node-a", "zen"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "uncordoned node-a: zen");

    let output = run_with_env(&["uncordon", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "uncordoned node-a: zen, openrouter");

    let output = run_with_env(
        &["--json", "uncordon", "node-a", "openrouter"],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), r#"{"removed":["openrouter"]}"#);

    let records = server.records.lock().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].path, "/v1/nodes/node-a/uncordon");
    assert_eq!(
        serde_json::from_slice::<Value>(&records[0].body).unwrap(),
        json!({"provider": "zen"})
    );
    assert!(records[1].body.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&records[2].body).unwrap(),
        json!({"provider": "openrouter"})
    );
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_prints_a_summary_and_json_prints_the_raw_body() {
    let body = json!({
        "version": "0.1.10",
        "uptime_secs": 42,
        "schema_version": 3,
        "db_bytes": 4096,
        "recovered_on_boot": 1,
        "jobs": {"queued": 2, "running": 1},
        "oldest_queued_secs": 7,
        "nodes": 3,
        "nodes_online": 2,
        "lease_secs": 60,
        "future": true
    });
    let expected = body.to_string();
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("status");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["status"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for (key, value) in [
        ("version", "0.1.10"),
        ("uptime_secs", "42"),
        ("schema_version", "3"),
        ("db_bytes", "4096"),
        ("recovered_on_boot", "1"),
        ("oldest_queued_secs", "7"),
        ("nodes", "3"),
        ("nodes_online", "2"),
        ("lease_secs", "60"),
    ] {
        assert!(
            text.lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>() == [key, value]),
            "{key}: {text}"
        );
    }
    let queued = text
        .lines()
        .find(|line| line.trim_start().starts_with("queued"))
        .unwrap();
    assert!(queued.trim_end().ends_with('2'), "{queued:?}");
    assert!(text.contains("STATE"), "{text}");

    let output = run_with_env(&["--json", "status"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
    assert_eq!(paths, vec!["/v1/status", "/v1/status"]);
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repos_and_sync_render_text_and_json() {
    let repos = json!({"repos": [
        {"name": "mirror", "url": "https://example.net/team/repo.git", "synced_at": 42, "heads": {"main": "abcdef"}},
        {"name": "idle", "url": "https://example.net/other.git", "synced_at": null, "heads": {}}
    ]});
    let synced = json!({"name": "mirror", "synced_at": 50, "heads": {"main": "abcdef"}});
    let repos_expected = repos.to_string();
    let sync_expected = synced.to_string();
    let server = start(move |record| {
        if record.path == "/v1/repos" {
            Reply::json(&repos)
        } else {
            Reply::json(&synced)
        }
    })
    .await;
    let dir = scratch("repos");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["repos"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let header = text.lines().next().unwrap();
    assert!(header.starts_with("NAME"), "{text}");
    assert!(header.contains("URL"), "{text}");
    assert!(header.contains("SYNCED"), "{text}");
    assert!(header.contains("HEADS"), "{text}");
    assert!(text.contains("mirror"), "{text}");
    assert!(text.contains("idle"), "{text}");

    let output = run_with_env(&["--json", "repos"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{repos_expected}\n"));

    let output = run_with_env(&["sync", "mirror"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["name", "mirror"]),
        "{text}"
    );
    assert!(text.contains("main"), "{text}");
    assert!(text.contains("abcdef"), "{text}");

    let output = run_with_env(&["--json", "sync", "mirror"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{sync_expected}\n"));

    let records = server.records.lock().unwrap();
    let calls: Vec<(&str, &str)> = records
        .iter()
        .map(|record| (record.method.as_str(), record.path.as_str()))
        .collect();
    assert_eq!(
        calls,
        vec![
            ("GET", "/v1/repos"),
            ("GET", "/v1/repos"),
            ("POST", "/v1/repos/mirror/sync"),
            ("POST", "/v1/repos/mirror/sync"),
        ]
    );
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prints_host_numbers_and_indented_process_tree() {
    let id = job_id();
    let stats = json!({
        "host": {
            "load1": 0.5,
            "load5": 0.25,
            "load15": 0.125,
            "mem_total_kb": 8192,
            "mem_available_kb": 4096,
            "swap_used_kb": 12,
            "disk_free_bytes": 250,
            "disk_total_bytes": 1000,
            "uptime_secs": 600
        },
        "labels": ["gpu", "php8.3"],
        "label_sources": {"php8.3": "probed"},
        "jobs": [{
            "job_id": id,
            "pgid": 4242,
            "output_idle_secs": 400,
            "cpu_idle_secs": 350,
            "stuck": true,
            "processes": [{
                "pid": 4242,
                "ppid": 1,
                "state": "S",
                "cpu_secs": 7,
                "rss_kb": 1024,
                "age_secs": 90,
                "cmdline": "opencode run\n\u{1b}[31m",
                "open_files": 3
            }]
        }]
    });
    let expected = stats.to_string();
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let node_view = json!({
        "name": "node-a",
        "cordon_details": [
            {
                "provider": "p1",
                "source": "failure",
                "reason": "Provider p1 failed: timeout",
                "set_at": wall - 10,
                "until": wall + 50,
                "lifts_after_healthy_heartbeats": null
            },
            {
                "provider": "p2",
                "source": "reiki",
                "reason": "Reiki cooldown for p2",
                "set_at": wall - 5,
                "until": null,
                "lifts_after_healthy_heartbeats": 2
            }
        ]
    });
    let server = start(move |record| {
        if record.path == "/v1/nodes" {
            Reply::json(&json!([node_view]))
        } else {
            Reply::json(&stats)
        }
    })
    .await;
    let dir = scratch("node");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["node", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("node  node-a"), "{text}");
    assert!(text.contains("labels gpu php8.3 (probed)"), "{text}");
    assert!(matches!(
        text.lines()
            .find(|line| line.starts_with("load1"))
            .and_then(|line| line.split_whitespace().nth(1)),
        Some("0.50")
    ));
    assert!(text.contains(&format!("job {id}")), "{text}");
    assert!(text.contains("stuck yes"), "{text}");
    assert!(text.contains("disk_free_percent  25"), "{text}");
    assert!(text.contains("cordons"), "{text}");
    let failure = text
        .lines()
        .find(|line| line.contains("p1 [failure]"))
        .unwrap();
    assert!(failure.contains("ago;"), "{failure:?}");
    assert!(failure.contains("left"), "{failure:?}");
    assert!(text.contains("Provider p1 failed: timeout"), "{text}");
    let reiki = text
        .lines()
        .find(|line| line.contains("p2 [reiki]"))
        .unwrap();
    assert!(reiki.contains("ago;"), "{reiki:?}");
    assert!(reiki.contains("lifts on 2 healthy"), "{reiki:?}");
    let process_line = text
        .lines()
        .find(|line| line.trim_start().starts_with("4242"))
        .unwrap();
    assert!(process_line.starts_with("  4242"), "{process_line:?}");
    assert!(
        process_line.contains("PID") || text.contains("  PID"),
        "{text}"
    );
    assert!(!text.contains('\u{1b}'), "{text:?}");

    let output = run_with_env(&["--json", "node", "node-a"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let records = server.records.lock().unwrap();
    let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "/v1/nodes/node-a/stats",
            "/v1/nodes",
            "/v1/nodes/node-a/stats"
        ]
    );
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_never_prints_the_token() {
    let id = job_id();
    let echo = format!("the token was {TOKEN}");
    let server = start(move |_| {
        let mut reply = Reply::json(&json!({"error": "unauthorized", "detail": echo}));
        reply.status = 401;
        reply
    })
    .await;
    let dir = scratch("unauthorized");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["show", &id], &env_for(&server, &token), None, &dir);
    assert_eq!(output.status.code(), Some(1));
    assert!(!stdout(&output).contains(TOKEN));
    assert!(!stderr(&output).contains(TOKEN), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unauthorized"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_exit_two_before_any_request() {
    let dir = scratch("usage");
    let output = run(&["show", "not-an-id"], &[], None, &dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("job id"), "{}", stderr(&output));

    let output = run(&["ls", "--state", "paused"], &[], None, &dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("unknown job state"),
        "{}",
        stderr(&output)
    );

    let output = run(
        &[
            "submit",
            "--repo",
            "/r",
            "--ref",
            "main",
            "--task",
            "a",
            "--task-file",
            "b",
        ],
        &[],
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

fn finished_job(id: &str, state: &str) -> Value {
    let mut job = job_json(id, state);
    job["spec"] = json!({
        "source": {"repo": "/srv/repo", "ref": "main"},
        "task": "go",
        "provider": "peer-a",
        "model": "old-model",
        "timeout_secs": 60,
        "idle_timeout_secs": 45,
        "idempotency_key": "ticket:7",
        "after": [id],
        "requires": ["old-label"],
        "fallbacks": [{"provider": "peer-x", "model": "old-fallback"}]
    });
    job
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_replaces_or_clears_label_requirements() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "failed");
    let created = job_json(&new_id, "queued");
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-require");
    let token = write_token(&dir, "operator.token", TOKEN);
    let env = env_for(&server, &token);

    let output = run_with_env(
        &["retry", &id, "--require", "php8.3", "--require", "gpu"],
        &env,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let output = run_with_env(&["retry", &id, "--no-require"], &env, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));

    let records = server.records.lock().unwrap();
    let posts: Vec<Value> = records
        .iter()
        .filter(|record| record.method == "POST")
        .map(|record| serde_json::from_slice(&record.body).unwrap())
        .collect();
    assert_eq!(posts[0]["requires"], json!(["php8.3", "gpu"]));
    assert!(posts[1].get("requires").is_none(), "{}", posts[1]);
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_replaces_or_clears_tolerations() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "failed");
    let created = job_json(&new_id, "queued");
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-tolerate");
    let token = write_token(&dir, "operator.token", TOKEN);
    let env = env_for(&server, &token);

    let output = run_with_env(
        &[
            "retry",
            &id,
            "--tolerate",
            "homelab",
            "--tolerate",
            "garage",
        ],
        &env,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let output = run_with_env(&["retry", &id, "--no-tolerate"], &env, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));

    let records = server.records.lock().unwrap();
    let posts: Vec<Value> = records
        .iter()
        .filter(|record| record.method == "POST")
        .map(|record| serde_json::from_slice(&record.body).unwrap())
        .collect();
    assert_eq!(posts[0]["tolerates"], json!(["homelab", "garage"]));
    assert!(posts[1].get("tolerates").is_none(), "{}", posts[1]);
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_replaces_or_clears_fallback_choices() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "failed");
    let created = job_json(&new_id, "queued");
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-fallback");
    let token = write_token(&dir, "operator.token", TOKEN);
    let env = env_for(&server, &token);

    let output = run_with_env(
        &[
            "retry",
            &id,
            "--fallback",
            "peer-c:new-model",
            "--fallback",
            "peer-d",
        ],
        &env,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let output = run_with_env(&["retry", &id, "--no-fallbacks"], &env, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));

    let records = server.records.lock().unwrap();
    let posts: Vec<Value> = records
        .iter()
        .filter(|record| record.method == "POST")
        .map(|record| serde_json::from_slice(&record.body).unwrap())
        .collect();
    assert_eq!(
        posts[0]["fallbacks"],
        json!([
            {"provider": "peer-c", "model": "new-model"},
            {"provider": "peer-d"},
        ])
    );
    assert!(posts[1].get("fallbacks").is_none(), "{}", posts[1]);
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_resubmits_the_finished_spec_and_applies_the_overrides() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "failed");
    let created = job_json(&new_id, "queued");
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry");
    let token = write_token(&dir, "operator.token", TOKEN);
    let dep = job_id();
    let output = run_with_env(
        &[
            "retry",
            &id,
            "--provider",
            "peer-b",
            "--model",
            "new-model",
            "--after",
            &dep,
            "--key",
            "ticket:42",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), new_id);
    let records = server.records.lock().unwrap();
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[0].path, format!("/v1/jobs/{id}"));
    assert_eq!(records[1].method, "POST");
    assert_eq!(records[1].path, "/v1/jobs");
    assert_eq!(records[1].header("idempotency-key"), Some("ticket:42"));
    let spec: Value = serde_json::from_slice(&records[1].body).unwrap();
    assert_eq!(spec["source"]["repo"], "/srv/repo");
    assert_eq!(spec["source"]["ref"], "main");
    assert_eq!(spec["task"], "go");
    assert_eq!(spec["harness"], "opencode");
    assert_eq!(spec["timeout_secs"], 60);
    assert_eq!(spec["idle_timeout_secs"], 45);
    assert_eq!(spec["provider"], "peer-b");
    assert_eq!(spec["model"], "new-model");
    assert_eq!(spec["after"], json!([dep]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_keeps_the_old_spec_and_sends_no_idempotency_key_by_default() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "cancelled");
    let created = job_json(&new_id, "queued");
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-plain");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["retry", &id], &env_for(&server, &token), None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), new_id);
    let records = server.records.lock().unwrap();
    assert_eq!(records[1].header("idempotency-key"), None);
    let spec: Value = serde_json::from_slice(&records[1].body).unwrap();
    assert!(spec.get("idempotency_key").is_none(), "{spec}");
    assert_eq!(spec["provider"], "peer-a");
    assert_eq!(spec["model"], "old-model");
    assert_eq!(spec["after"], json!([id]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_resolves_after_key_to_a_dependency() {
    let old = job_id();
    let dep = job_id();
    let new_id = job_id();
    let finished = finished_job(&old, "failed");
    let dep_id = dep.clone();
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&job_json(&new_id, "queued"))
        } else if record.query.is_some() {
            Reply::json(&json!({"jobs": [job_json(&dep_id, "done")], "next": null}))
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-after-key");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["retry", &old, "--after-key", "build"],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let records = server.records.lock().unwrap();
    let post = records.iter().find(|r| r.method == "POST").unwrap();
    let spec: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(spec["after"], json!([dep]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_refuses_a_job_that_is_not_finished() {
    let id = job_id();
    let body = job_json(&id, "running");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("retry-not-terminal");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(&["retry", &id], &env_for(&server, &token), None, &dir);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("retry resubmits a finished job"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        server.records.lock().unwrap().len(),
        1,
        "a refusal must not submit anything"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_json_prints_the_raw_submit_body() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "done");
    let created = job_json(&new_id, "queued");
    let expected = created.to_string();
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&created)
        } else {
            Reply::json(&finished)
        }
    })
    .await;
    let dir = scratch("retry-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["--json", "retry", &id],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_wait_reports_the_new_jobs_state_and_exit_code() {
    let id = job_id();
    let new_id = job_id();
    let finished = finished_job(&id, "done");
    let post_id = new_id.clone();
    let poll_id = new_id.clone();
    let old_path = id.clone();
    let server = start(move |record| {
        if record.method == "POST" {
            Reply::json(&job_json(&post_id, "queued"))
        } else if record.path == format!("/v1/jobs/{old_path}") {
            Reply::json(&finished)
        } else {
            Reply::json(&job_json(&poll_id, "failed"))
        }
    })
    .await;
    let dir = scratch("retry-wait");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["retry", &id, "--wait"], &envs, None, &dir);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), new_id);
    assert!(
        stderr(&output).contains("state: failed"),
        "{}",
        stderr(&output)
    );

    let output = run_with_env(&["--json", "retry", &id, "--wait"], &envs, None, &dir);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(&new_id), "{text}");
    assert!(text.contains(r#""state":"failed""#), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_sends_the_patch_to_the_edit_route() {
    let id = job_id();
    let dep = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("edit-patch");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "edit",
            &id,
            "--provider",
            "peer-b",
            "--model",
            "new-model",
            "--fallback",
            "peer-c:fallback-model",
            "--require",
            "gpu",
            "--tolerate",
            "homelab",
            "--after",
            &dep,
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), format!("{id} queued"));
    let records = server.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].path, format!("/v1/jobs/{id}/edit"));
    let patch: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(patch["provider"], "peer-b");
    assert_eq!(patch["model"], "new-model");
    assert_eq!(
        patch["fallbacks"],
        json!([{"provider": "peer-c", "model": "fallback-model"}])
    );
    assert_eq!(patch["requires"], json!(["gpu"]));
    assert_eq!(patch["tolerates"], json!(["homelab"]));
    assert_eq!(patch["after"], json!([dep]));
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_sends_empty_lists_for_the_no_flags_and_resolves_after_key() {
    let id = job_id();
    let dep = job_id();
    let body = job_json(&id, "blocked");
    let dep_id = dep.clone();
    let server = start(move |record| {
        if record.query.is_some() {
            Reply::json(&json!({"jobs": [job_json(&dep_id, "queued")], "next": null}))
        } else {
            Reply::json(&body)
        }
    })
    .await;
    let dir = scratch("edit-clear");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "edit",
            &id,
            "--no-fallbacks",
            "--no-require",
            "--no-tolerate",
            "--after-key",
            "build",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), format!("{id} blocked"));
    let records = server.records.lock().unwrap();
    let post = records.iter().find(|r| r.path.ends_with("/edit")).unwrap();
    let patch: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(patch["fallbacks"], json!([]));
    assert_eq!(patch["requires"], json!([]));
    assert_eq!(patch["tolerates"], json!([]));
    assert_eq!(patch["after"], json!([dep]));
    assert!(patch.get("provider").is_none());
    drop(records);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_json_prints_the_raw_body() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let expected = body.to_string();
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("edit-json");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &["--json", "edit", &id, "--model", "peer-model"],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_refuses_any_with_a_model_or_fallback() {
    let id = job_id();
    let reply_id = id.clone();
    let server = start(move |_| Reply::json(&job_json(&reply_id, "queued"))).await;
    let dir = scratch("edit-any-reject");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);
    for extra in [vec!["--model", "some-model"], vec!["--fallback", "peer-b"]] {
        let mut args = vec!["edit", &id, "--provider", "any"];
        args.extend(extra);
        let output = run_with_env(&args, &envs, None, &dir);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("--provider any cannot be combined"),
            "{}",
            stderr(&output)
        );
    }
    assert!(server.records.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_accepts_any_and_rejects_a_model_or_fallback() {
    let id = job_id();
    let body = job_json(&id, "queued");
    let server = start(move |_| Reply::json(&body)).await;
    let dir = scratch("submit-any");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);
    let base = [
        "submit",
        "--repo",
        "/r",
        "--ref",
        "main",
        "--provider",
        "any",
    ];

    let output = run_with_env(&base, &envs, Some("do it\n"), &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let records = server.records.lock().unwrap();
    let spec: Value = serde_json::from_slice(&records[0].body).unwrap();
    assert_eq!(spec["provider"], "any");
    assert!(spec.get("model").is_none());
    drop(records);

    for extra in [vec!["--model", "m"], vec!["--fallback", "peer-b"]] {
        let mut args = base.to_vec();
        args.extend(extra);
        let output = run_with_env(&args, &envs, Some("do it\n"), &dir);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("provider is \"any\""),
            "{}",
            stderr(&output)
        );
    }
    assert_eq!(server.records.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

fn collected_view(id: &str, state: &str, diff_bytes: u64) -> Value {
    let mut view = job_json(id, state);
    view["diff_bytes"] = json!(diff_bytes);
    view
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collect_resolves_keys_and_ids_and_sets_the_collection_spec() {
    let build = job_id();
    let test = job_id();
    let direct = job_id();
    let new_id = job_id();
    let entries = [
        ("build".to_owned(), build.clone()),
        ("test".to_owned(), test.clone()),
    ]
    .to_vec();
    let views = [
        collected_view(&build, "done", 120),
        collected_view(&test, "failed", 40),
        collected_view(&direct, "done", 10),
    ]
    .to_vec();
    let reply_id = new_id.clone();
    let test_id = test.clone();
    let server = start(move |record| {
        if record.method == "GET" {
            if let Some(id) = record.path.strip_prefix("/v1/jobs/") {
                let view = views
                    .iter()
                    .find(|view| view["id"] == json!(id))
                    .cloned()
                    .unwrap_or_else(|| job_json(id, "done"));
                return Reply::json(&view);
            }
            let key = record
                .query
                .as_deref()
                .and_then(|q| q.strip_prefix("key="))
                .unwrap_or("");
            let jobs: Vec<Value> = entries
                .iter()
                .filter(|(name, _)| name == key)
                .map(|(_, id)| {
                    collected_view(
                        id,
                        if id == &test_id { "failed" } else { "done" },
                        if id == &test_id { 40 } else { 120 },
                    )
                })
                .collect();
            return Reply::json(&json!({"jobs": jobs, "next": null}));
        }
        Reply::json(&job_json(&reply_id, "blocked"))
    })
    .await;
    let dir = scratch("collect-resolve");
    let token = write_token(&dir, "operator.token", TOKEN);
    let mut envs = env_for(&server, &token);
    envs.push((
        "XDG_CONFIG_HOME".to_owned(),
        dir.join("no-config").display().to_string(),
    ));
    let output = run_with_env(
        &[
            "collect",
            "build",
            "test",
            &direct,
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--template",
            "test",
            "--key",
            "combined",
            "--provider",
            "peer-a",
            "--model",
            "m1",
            "--timeout",
            "90",
        ],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), new_id);
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap().last().unwrap().body)
        .expect("POST body");
    assert_eq!(spec["after"], json!([build, test, direct]));
    assert_eq!(spec["collect"], json!("both"));
    assert_eq!(spec["on_conflict"], json!("markers"));
    assert_eq!(spec["include_failed"], json!(true));
    assert_eq!(spec["idempotency_key"], json!("combined"));
    assert_eq!(spec["provider"], json!("peer-a"));
    assert_eq!(spec["model"], json!("m1"));
    assert_eq!(spec["timeout_secs"], json!(90));
    let task = spec["task"].as_str().unwrap();
    assert!(task.contains("Run the repository's checks"), "{task}");
    assert!(!task.contains("## Extra instructions"), "{task}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collect_refuses_scrubbed_patches_unless_explicitly_allowed() {
    let dep = job_id();
    let new_id = job_id();
    let dep_id = dep.clone();
    let reply_id = new_id.clone();
    let server = start(move |record| {
        if record.method == "GET" {
            let mut view = collected_view(&dep_id, "done", 10);
            view["patch_scrubbed"] = json!(true);
            view["scrubbed_hits"] = json!(2);
            if record.path.starts_with("/v1/jobs/") {
                return Reply::json(&view);
            }
            return Reply::json(&json!({"jobs": [view], "next": null}));
        }
        Reply::json(&job_json(&reply_id, "blocked"))
    })
    .await;
    let dir = scratch("collect-scrubbed");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let refused = run_with_env(
        &[
            "collect",
            &dep,
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
        ],
        &envs,
        None,
        &dir,
    );
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("--allow-scrubbed"),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("2 scrubbed value(s)"),
        "{}",
        stderr(&refused)
    );
    assert!(
        !server
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "POST"),
        "a refused collect must not submit a job"
    );

    let allowed = run_with_env(
        &[
            "collect",
            &dep,
            "--repo",
            "https://example.net/repo.git",
            "--ref",
            "main",
            "--allow-scrubbed",
        ],
        &envs,
        None,
        &dir,
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
    assert_eq!(stdout(&allowed).trim(), new_id);
    assert!(server
        .records
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.method == "POST"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collect_reads_template_override_and_appends_task_file() {
    let dep = job_id();
    let new_id = job_id();
    let dep_id = dep.clone();
    let server = start(move |record| {
        if record.method == "GET" {
            return Reply::json(&json!({
                "jobs": [collected_view(&dep_id, "done", 10)],
                "next": null
            }));
        }
        Reply::json(&job_json(&new_id, "blocked"))
    })
    .await;
    let dir = scratch("collect-override");
    let token = write_token(&dir, "operator.token", TOKEN);
    let template = dir.join("lu").join("templates").join("merge.md");
    std::fs::create_dir_all(template.parent().unwrap()).unwrap();
    std::fs::write(&template, "OVERRIDE PROMPT\n").unwrap();
    let extra = dir.join("extra.md");
    std::fs::write(&extra, "Extra line\n").unwrap();
    let mut envs = env_for(&server, &token);
    envs.push(("XDG_CONFIG_HOME".to_owned(), dir.display().to_string()));
    let output = run_with_env(
        &[
            "collect",
            "the-key",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--task-file",
            extra.to_str().unwrap(),
        ],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let spec: Value = serde_json::from_slice(&server.records.lock().unwrap().last().unwrap().body)
        .expect("POST body");
    assert_eq!(
        spec["task"],
        json!("OVERRIDE PROMPT\n\n## Extra instructions\n\nExtra line\n")
    );
    assert_eq!(spec["after"], json!([dep]));
    assert!(spec["include_failed"].is_null(), "{spec}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collect_unknown_key_is_a_usage_error_without_submitting() {
    let new_id = job_id();
    let server = start(move |record| {
        if record.method == "GET" {
            return Reply::json(&json!({"jobs": [], "next": null}));
        }
        Reply::json(&job_json(&new_id, "blocked"))
    })
    .await;
    let dir = scratch("collect-missing-key");
    let token = write_token(&dir, "operator.token", TOKEN);
    let output = run_with_env(
        &[
            "collect",
            "no-such-key",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no-such-key"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(server
        .records
        .lock()
        .unwrap()
        .iter()
        .all(|record| record.method == "GET"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collect_enforces_template_and_task_file_caps() {
    let dir = scratch("collect-caps");
    let token = write_token(&dir, "operator.token", TOKEN);
    let template = dir.join("lu").join("templates").join("merge.md");
    std::fs::create_dir_all(template.parent().unwrap()).unwrap();
    std::fs::write(&template, "x".repeat(64 * 1024 + 1)).unwrap();
    let server = server_for_caps().await;
    let mut envs = env_for(&server, &token);
    envs.push(("XDG_CONFIG_HOME".to_owned(), dir.display().to_string()));
    let output = run_with_env(
        &[
            "collect",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
        ],
        &envs,
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("larger than 65536 bytes"),
        "{}",
        stderr(&output)
    );

    std::fs::write(&template, "ok\n").unwrap();
    let task_file = dir.join("huge.md");
    std::fs::write(&task_file, "y".repeat(256 * 1024 + 1)).unwrap();
    let output = run_with_env(
        &[
            "collect",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--repo",
            "/srv/git/r",
            "--ref",
            "main",
            "--task-file",
            task_file.to_str().unwrap(),
        ],
        &envs,
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("larger than 262144 bytes"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

async fn server_for_caps() -> TestServer {
    start(|_| Reply::json(&job_json(&job_id(), "done"))).await
}

#[tokio::test]
async fn audit_cli_verifies_tails_and_prints_the_head() {
    use lu_audit::{Data, Event, Field};

    let dir = scratch("audit-cli");
    let path = dir.join("audit.jsonl");
    let errors = Arc::new(AtomicU64::new(0));
    let (writer, task) = lu_audit::Writer::open(&path, Arc::clone(&errors))
        .await
        .unwrap();
    writer.record(
        Event::Submitted,
        Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        None,
        "operator",
        Data::new().text(Field::Provider, "reiki"),
    );
    writer.record(
        Event::Started,
        Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        Some("node-a"),
        "node-a",
        Data::new(),
    );
    drop(writer);
    task.await.unwrap();
    let file = path.to_str().unwrap();

    let out = run_with_env(&["audit", "verify", file], &[], None, &dir);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("ok: 2 record(s)"),
        "{:?}",
        stdout(&out)
    );

    let out = run_with_env(&["--json", "audit", "verify", file], &[], None, &dir);
    let value: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["records"], 2);

    let out = run_with_env(&["audit", "head", file], &[], None, &dir);
    assert!(out.status.success(), "{}", stderr(&out));
    let head = stdout(&out).trim().to_owned();
    assert_eq!(head.len(), 64);
    assert!(head.bytes().all(|b| b.is_ascii_hexdigit()));

    let out = run_with_env(&["audit", "tail", "--lines", "1", file], &[], None, &dir);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("\"event\":\"started\""), "{text}");
    assert!(!text.contains("submitted"), "{text}");

    // Anchoring detects removed trailing records even though the remaining chain is valid.
    let out = run_with_env(
        &["audit", "verify", file, "--head", &format!("{head}0")],
        &[],
        None,
        &dir,
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("head hash"), "{}", stderr(&out));

    // A tampered line fails verification.
    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    std::fs::write(
        &path,
        text.replacen("\"event\":\"started\"", "\"event\":\"failed\"", 1),
    )
    .unwrap();
    let out = run_with_env(&["audit", "verify", file], &[], None, &dir);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("hash mismatch"), "{}", stderr(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_batch_dry_run_prints_the_plan_without_connecting() {
    let server = start(|_| Reply::status(500)).await;
    let dir = scratch("submit-batch-dry-run");
    let tasks = dir.join("tasks.md");
    let defaults = dir.join("defaults.toml");
    std::fs::write(
        &tasks,
        "# Work\n- [ ] First task\n- [ ] Second task\n  - [ ] nested detail\n",
    )
    .unwrap();
    std::fs::write(&defaults, "repo = \"/srv/repo\"\nref = \"main\"\n").unwrap();
    let missing_token = dir.join("does-not-exist.token");
    let env = vec![
        ("LU_URL".to_owned(), server.url.clone()),
        (
            "LU_TOKEN_FILE".to_owned(),
            missing_token.display().to_string(),
        ),
    ];
    let tasks_arg = tasks.display().to_string();
    let defaults_arg = defaults.display().to_string();
    let output = run_with_env(
        &[
            "--json",
            "submit",
            "--batch",
            &tasks_arg,
            "--key-prefix",
            "plan",
            "--defaults",
            &defaults_arg,
            "--chain",
            "--dry-run",
        ],
        &env,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let plan: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["chain"], true);
    assert_eq!(plan["max_parallel"], Value::Null);
    let jobs = plan["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0]["key"], "plan-first-task");
    assert_eq!(jobs[0]["after"], json!([]));
    assert_eq!(jobs[1]["key"], "plan-second-task");
    assert_eq!(jobs[1]["after"], json!(["plan-first-task"]));
    assert!(server.records.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_batch_is_idempotent_and_caps_the_group() {
    let ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = Arc::clone(&ids);
    let server = start(
        move |record| match (record.method.as_str(), record.path.as_str()) {
            ("PUT", path) if path.ends_with("/limit") => {
                Reply::json(&json!({"group": "shared", "max_running": 2}))
            }
            ("POST", "/v1/jobs") => {
                let spec: Value = serde_json::from_slice(&record.body).unwrap();
                let id = job_id();
                recorded.lock().unwrap().push(id.clone());
                let mut job = job_json(&id, "queued");
                job["spec"] = spec;
                Reply::json(&job)
            }
            _ => Reply::status(404),
        },
    )
    .await;
    let dir = scratch("submit-batch-idempotent");
    let tasks = dir.join("tasks");
    std::fs::create_dir(&tasks).unwrap();
    std::fs::write(tasks.join("build.md"), "Build the thing").unwrap();
    std::fs::write(tasks.join("test.md"), "Test the thing").unwrap();
    let defaults = dir.join("defaults.toml");
    std::fs::write(
        &defaults,
        "repo = \"/srv/repo\"\nref = \"main\"\ngroup = \"shared\"\n",
    )
    .unwrap();
    let token = write_token(&dir, "operator.token", TOKEN);
    let env = env_for(&server, &token);
    let tasks_arg = tasks.display().to_string();
    let defaults_arg = defaults.display().to_string();
    let args = [
        "--json",
        "submit",
        "--batch",
        &tasks_arg,
        "--key-prefix",
        "sprint",
        "--defaults",
        &defaults_arg,
        "--chain",
        "--max-parallel",
        "2",
    ];
    for _ in 0..2 {
        let output = run_with_env(&args, &env, None, &dir);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(!stdout(&output).contains(TOKEN));
        assert!(!stderr(&output).contains(TOKEN));
        let result: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
        assert_eq!(result["submitted_count"], 2);
        assert_eq!(result["jobs"][0]["key"], "sprint-build");
        assert_eq!(result["jobs"][1]["key"], "sprint-test");
    }
    let bodies = server
        .records
        .lock()
        .unwrap()
        .iter()
        .filter(|record| record.method == "POST" && record.path == "/v1/jobs")
        .map(|record| serde_json::from_slice::<Value>(&record.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(bodies.len(), 4);
    let keys = bodies
        .iter()
        .map(|body| body["idempotency_key"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec!["sprint-build", "sprint-test", "sprint-build", "sprint-test"]
    );
    let ids = ids.lock().unwrap();
    assert_eq!(bodies[1]["after"], json!([ids[0]]));
    assert_eq!(bodies[3]["after"], json!([ids[2]]));
    for body in &bodies {
        assert_eq!(body["group"], "shared");
    }
    let puts = server
        .records
        .lock()
        .unwrap()
        .iter()
        .filter(|record| record.method == "PUT")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[0].path, "/v1/groups/shared/limit");
    assert_eq!(
        serde_json::from_slice::<Value>(&puts[0].body).unwrap(),
        json!({"max_running": 2})
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_batch_lists_every_validation_error_before_submitting() {
    let server = start(|_| Reply::status(500)).await;
    let dir = scratch("submit-batch-errors");
    let checklist = dir.join("tasks.md");
    std::fs::write(&checklist, "- [ ] Duplicate\n- [ ] Duplicate\n- [ ]    \n").unwrap();
    let token = write_token(&dir, "operator.token", TOKEN);
    let checklist_arg = checklist.display().to_string();
    let output = run_with_env(
        &[
            "submit",
            "--batch",
            &checklist_arg,
            "--key-prefix",
            "bad",
            "--repo",
            "/srv/repo",
            "--ref",
            "main",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    let errors = stderr(&output);
    assert!(errors.contains("duplicate job key"), "{errors}");
    assert!(errors.contains("task is empty"), "{errors}");
    assert!(errors.contains("cannot derive a job slug"), "{errors}");
    assert!(server.records.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_batch_maps_sidecars_external_keys_and_nested_after_hints() {
    let dependency = job_id();
    let dep_id = dependency.clone();
    let ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = Arc::clone(&ids);
    let server = start(
        move |record| match (record.method.as_str(), record.path.as_str()) {
            ("GET", "/v1/jobs") => {
                let mut job = job_json(&dep_id, "done");
                job["spec"]["idempotency_key"] = json!("external-key");
                Reply::json(&json!({"jobs": [job], "next": null}))
            }
            ("POST", "/v1/jobs") => {
                let spec: Value = serde_json::from_slice(&record.body).unwrap();
                let id = job_id();
                recorded.lock().unwrap().push(id.clone());
                let mut job = job_json(&id, "queued");
                job["spec"] = spec;
                Reply::json(&job)
            }
            _ => Reply::status(404),
        },
    )
    .await;
    let dir = scratch("submit-batch-sidecar");
    let checklist = dir.join("review.md");
    std::fs::write(
        &checklist,
        "- [ ] Build\n  after: external-key\n- [ ] Test\n  after: build\n- [x] Already done\n```\n- [ ] Inside a fence\n```\n",
    )
    .unwrap();
    let sidecar = dir.join("review.toml");
    std::fs::write(
        &sidecar,
        "provider = \"codex\"\nmodel = \"gpt\"\npriority = 3\ngroup = \"batch\"\ntimeout = 120\nworkspace = \"keep\"\nrequire = [\"linux\"]\nexpect = { files = [\"REVIEW.md\"], mode = \"fail\" }\n",
    )
    .unwrap();
    let token = write_token(&dir, "operator.token", TOKEN);
    let checklist_arg = checklist.display().to_string();
    let output = run_with_env(
        &[
            "submit",
            "--batch",
            &checklist_arg,
            "--key-prefix",
            "review",
            "--repo",
            "/srv/repo",
            "--ref",
            "main",
        ],
        &env_for(&server, &token),
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let bodies = server
        .records
        .lock()
        .unwrap()
        .iter()
        .filter(|record| record.method == "POST" && record.path == "/v1/jobs")
        .map(|record| serde_json::from_slice::<Value>(&record.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(bodies.len(), 2);
    let ids = ids.lock().unwrap();
    assert_eq!(bodies[0]["after"], json!([dependency]));
    assert_eq!(bodies[1]["after"], json!([ids[0]]));
    for body in &bodies {
        assert_eq!(body["provider"], "codex");
        assert_eq!(body["model"], "gpt");
        assert_eq!(body["priority"], 3);
        assert_eq!(body["group"], "batch");
        assert_eq!(body["timeout_secs"], 120);
        assert_eq!(body["workspace"], "keep");
        assert_eq!(body["requires"], json!(["linux"]));
        assert_eq!(body["expect"]["files"], json!(["REVIEW.md"]));
        assert_eq!(body["expect"]["mode"], "fail");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
