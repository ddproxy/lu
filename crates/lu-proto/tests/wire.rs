//! End-to-end use of the public API: a job goes out as an assignment, comes back as log chunks
//! and a completion, while a heartbeat reports the node. Nothing here is inside the crate, so it
//! also checks that the re-exports are usable.

use std::str::FromStr;

use lu_proto::{
    printable, truncate_chars, AgentEvent, ArtifactName, Choice, Completion, FailureClass,
    HarnessState, Heartbeat, JobId, JobSpec, JobState, JobView, LogChunk, NodeName, NodeView,
    Outcome, ProviderModel, ProviderState, ProviderStatus,
};

fn job_id() -> JobId {
    JobId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

fn spec() -> JobSpec {
    JobSpec {
        repo: "https://example.net/team/repo.git".to_owned(),
        git_ref: "main".to_owned(),
        base_commit: None,
        task: "add the missing test".to_owned(),
        harness: "opencode".to_owned(),
        provider: Some("peer-a".to_owned()),
        model: None,
        timeout_secs: 600,
        idle_timeout_secs: Some(60),
        idempotency_key: Some("ticket:42".to_owned()),
        after: vec![JobId::new()],
        stack_dependencies: true,
        collect: None,
        include_failed: false,
        allow_empty: false,
        requires: vec!["php8.3".to_owned()],
        tolerates: Vec::new(),
        fallbacks: vec![lu_proto::Choice {
            provider: "peer-b".to_owned(),
            model: Some("other-model".to_owned()),
        }],
    }
}

#[test]
fn a_submission_travels_as_an_assignment_and_comes_back_as_a_completion() {
    let spec = spec();
    spec.validate().unwrap();

    let assign = AgentEvent::Assign {
        job_id: job_id(),
        epoch: 7,
        spec: spec.clone(),
        provider: Some("peer-b".to_owned()),
        model: Some("other-model".to_owned()),
    };
    let wire = serde_json::to_string(&assign).unwrap();
    assert_eq!(serde_json::from_str::<AgentEvent>(&wire).unwrap(), assign);

    let chunk = LogChunk {
        job_id: job_id(),
        epoch: 7,
        seq: 1,
        data: "building...\n".to_owned(),
    };
    chunk.validate().unwrap();
    assert_eq!(
        serde_json::from_str::<LogChunk>(&serde_json::to_string(&chunk).unwrap()).unwrap(),
        chunk
    );

    let completion = Completion {
        job_id: job_id(),
        epoch: 7,
        outcome: Outcome::Failed(FailureClass::Execution),
        exit_code: Some(101),
        summary: Some("harness exited non-zero".to_owned()),
        retry_after_secs: None,
    };
    completion.validate().unwrap();
    assert_eq!(
        serde_json::from_str::<Completion>(&serde_json::to_string(&completion).unwrap()).unwrap(),
        completion
    );
    assert!(!FailureClass::Execution.retryable(1, 1));
    assert!(FailureClass::Node.retryable(1, 1));
}

#[test]
fn a_heartbeat_carries_a_capped_forwarded_document() {
    let beat = Heartbeat {
        node: NodeName::from_str("peer-a").unwrap(),
        harness: HarnessState::Idle,
        version: Some("0.1.10".to_owned()),
        providers: vec![ProviderStatus {
            name: "peer-a".to_owned(),
            state: ProviderState::Cooling,
            soonest_secs: Some(15),
        }],
        provider_models: Vec::new(),
        reiki: Some(serde_json::json!({"providers": ["peer-a"]})),
        host: None,
        jobs: Vec::new(),
        labels: vec!["gpu".to_owned()],
        label_sources: std::collections::BTreeMap::new(),
        taints: Vec::new(),
    };
    beat.validate().unwrap();
    let wire = serde_json::to_string(&beat).unwrap();
    assert_eq!(serde_json::from_str::<Heartbeat>(&wire).unwrap(), beat);
}

#[test]
fn artifact_paths_come_only_from_the_job_id_and_the_fixed_names() {
    let id = job_id();
    assert_eq!(
        ArtifactName::RunLog.relative_path(&id),
        format!("{id}/run.log")
    );
    assert_eq!(ArtifactName::ALL.len(), 3);
}

#[test]
fn a_lifecycle_is_checked_against_the_transition_table() {
    assert!(JobState::can_transition(JobState::Queued, JobState::Leased));
    assert!(JobState::can_transition(
        JobState::Running,
        JobState::Recovering
    ));
    assert!(!JobState::can_transition(JobState::Done, JobState::Running));
    assert!(JobState::Done.is_terminal());
}

#[test]
fn display_text_is_sanitised_and_capped() {
    let hostile = "peer-a\u{1b}[2J\u{202e}\u{0}ok";
    let shown = printable(hostile);
    assert!(!shown.contains('\u{1b}'));
    assert!(!shown.contains('\u{202e}'));
    assert!(!shown.contains('\u{0}'));
    assert_eq!(truncate_chars(&shown, 3).chars().count(), 3);
}

#[test]
fn job_and_node_views_match_the_documented_json() {
    let dep = JobId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAA").unwrap();
    let job = JobView {
        id: job_id(),
        state: JobState::Running,
        spec: JobSpec {
            repo: "https://example.net/team/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            task: "add the missing test".to_owned(),
            provider: Some("peer-a".to_owned()),
            timeout_secs: 600,
            idle_timeout_secs: Some(60),
            idempotency_key: Some("ticket:42".to_owned()),
            after: vec![dep.clone()],
            requires: vec!["php8.3".to_owned()],
            fallbacks: vec![Choice {
                provider: "peer-b".to_owned(),
                model: Some("other-model".to_owned()),
            }],
            ..spec()
        },
        attempt: 2,
        node: Some(NodeName::from_str("peer-a").unwrap()),
        assigned_provider: Some("peer-b".to_owned()),
        assigned_model: Some("other-model".to_owned()),
        epoch: 7,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_010,
        started_at: Some(1_700_000_020),
        finished_at: None,
        failure_class: None,
        summary: None,
        exit_code: None,
        artifacts: vec!["diff.patch".to_owned(), "run.log".to_owned()],
        cancel_requested: false,
        after: vec![dep.clone()],
        waiting_on: vec![dep],
        waiting_reason: None,
        outcome_note: None,
        warnings: Some(Vec::new()),
    };
    let expected_job = serde_json::json!({
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "state": "running",
        "spec": {
            "repo": "https://example.net/team/repo.git",
            "git_ref": "main",
            "task": "add the missing test",
            "harness": "opencode",
            "provider": "peer-a",
            "timeout_secs": 600,
            "idle_timeout_secs": 60,
            "idempotency_key": "ticket:42",
            "after": ["01ARZ3NDEKTSV4RRFFQ69G5FAA"],
            "requires": ["php8.3"],
            "fallbacks": [{"provider": "peer-b", "model": "other-model"}]
        },
        "attempt": 2,
        "node": "peer-a",
        "assigned_provider": "peer-b",
        "assigned_model": "other-model",
        "epoch": 7,
        "created_at": 1_700_000_000u64,
        "updated_at": 1_700_000_010u64,
        "started_at": 1_700_000_020u64,
        "finished_at": null,
        "failure_class": null,
        "summary": null,
        "exit_code": null,
        "artifacts": ["diff.patch", "run.log"],
        "cancel_requested": false,
        "after": ["01ARZ3NDEKTSV4RRFFQ69G5FAA"],
        "waiting_on": ["01ARZ3NDEKTSV4RRFFQ69G5FAA"],
        "waiting_reason": null,
        "warnings": []
    });
    assert_eq!(serde_json::to_value(&job).unwrap(), expected_job);

    let node = NodeView {
        name: NodeName::from_str("peer-a").unwrap(),
        version: Some("0.1.10".to_owned()),
        last_seen: 1_700_000_030,
        online: true,
        draining: false,
        harness: Some(HarnessState::Idle),
        providers: vec![ProviderStatus {
            name: "peer-b".to_owned(),
            state: ProviderState::Ok,
            soonest_secs: None,
        }],
        provider_models: vec![ProviderModel {
            provider: "peer-b".to_owned(),
            model: "other-model".to_owned(),
        }],
        labels: vec!["gpu".to_owned()],
        label_sources: std::collections::BTreeMap::from([("gpu".to_owned(), "probed".to_owned())]),
        taints: vec!["homelab".to_owned()],
        current_job: Some(job_id()),
        cordons: vec!["peer-c".to_owned()],
        reiki: Some(serde_json::json!({"status": "ok"})),
        load1: Some(1.5),
        mem_available_kb: Some(8192),
        mem_total_kb: Some(16_384),
        stuck: 1,
    };
    let expected_node = serde_json::json!({
        "name": "peer-a",
        "version": "0.1.10",
        "last_seen": 1_700_000_030u64,
        "online": true,
        "draining": false,
        "harness": "idle",
        "providers": [{"name": "peer-b", "state": "ok"}],
        "provider_models": [{"provider": "peer-b", "model": "other-model"}],
        "labels": ["gpu"],
        "label_sources": {"gpu": "probed"},
        "taints": ["homelab"],
        "current_job": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "cordons": ["peer-c"],
        "reiki": {"status": "ok"},
        "load1": 1.5,
        "mem_available_kb": 8192,
        "mem_total_kb": 16_384,
        "stuck": 1
    });
    assert_eq!(serde_json::to_value(&node).unwrap(), expected_node);

    for value in [expected_job, expected_node] {
        let text = serde_json::to_string(&value).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            value
        );
    }
}
