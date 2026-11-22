//! Draft CLI coverage: listing, provenance display, approval with an inline edit and rejection.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;

fn draft(id: &str) -> Value {
    let mut view = job_json(id, "draft");
    view["spec"]["origin"] = json!("generic-inbox");
    view["spec"]["origin_ref"] = json!("delivery-42");
    view["spec"]["untrusted_context"] = json!(true);
    view["spec"]["context"] = json!("body: untrusted words");
    view
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drafts_are_listed_shown_approved_and_rejected() {
    let id = job_id();
    let draft_view = draft(&id);
    let approved = job_json(&id, "queued");
    let rejected = job_json(&id, "cancelled");
    let server = start(move |record| {
        if record.path == "/v1/drafts" {
            Reply::json(&json!({"drafts": [draft_view.clone()], "next": null}))
        } else if record.path.ends_with("/approve") {
            Reply::json(&approved)
        } else if record.path.ends_with("/reject") {
            Reply::json(&rejected)
        } else {
            Reply::json(&draft_view)
        }
    })
    .await;
    let dir = scratch("drafts-cli");
    let token = write_token(&dir, "operator.token", TOKEN);
    let envs = env_for(&server, &token);

    let output = run_with_env(&["--json", "drafts", "--limit", "5"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let listed: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["spec"]["origin"], "generic-inbox");
    assert_eq!(server.records.lock().unwrap()[0].path, "/v1/drafts");

    let output = run_with_env(&["--json", "show", &id], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let shown: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(shown["spec"]["origin"], "generic-inbox");
    assert_eq!(shown["spec"]["origin_ref"], "delivery-42");
    assert_eq!(shown["spec"]["untrusted_context"], true);

    let task_file = dir.join("edited-task.txt");
    fs::write(&task_file, "operator-approved task").unwrap();
    let task_arg = task_file.to_str().unwrap();
    let output = run_with_env(
        &[
            "approve",
            &id,
            "--edit",
            "--task-file",
            task_arg,
            "--priority",
            "3",
        ],
        &envs,
        None,
        &dir,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("queued"));
    let records = server.records.lock().unwrap();
    let approval = records
        .iter()
        .find(|record| record.path.ends_with("/approve"))
        .expect("approval request");
    let sent: Value = serde_json::from_slice(&approval.body).unwrap();
    assert_eq!(sent["edit"]["task"], "operator-approved task");
    assert_eq!(sent["edit"]["priority"], 3);
    drop(records);

    let output = run_with_env(&["reject", &id, "--reason", "duplicate"], &envs, None, &dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("cancelled"));
    let records = server.records.lock().unwrap();
    let rejection = records
        .iter()
        .find(|record| record.path.ends_with("/reject"))
        .expect("rejection request");
    let sent: Value = serde_json::from_slice(&rejection.body).unwrap();
    assert_eq!(sent["reason"], "duplicate");

    let _ = fs::remove_dir_all(&dir);
}
