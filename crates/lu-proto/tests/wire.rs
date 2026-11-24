//! End-to-end use of the public API: a job goes out as an assignment, comes back as log chunks
//! and a completion, while a heartbeat reports the node. Nothing here is inside the crate, so it
//! also checks that the re-exports are usable.

use std::str::FromStr;

use lu_schedule::{BlockReason, Constraint, JobSchedule, OnDeadline};

use lu_proto::{
    printable, truncate_chars, AgentEvent, ArtifactName, Choice, Completion, CordonView,
    FailureClass, HarnessState, Heartbeat, JobId, JobSpec, JobState, JobView, LogChunk, NodeHealth,
    NodeHealthLevel, NodeName, NodeView, Outcome, ProviderModel, ProviderState, ProviderStatus,
    WorkspacePolicy, WorkspaceState, WorkspaceView,
};

fn job_id() -> JobId {
    JobId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

fn spec() -> JobSpec {
    JobSpec {
        source: lu_proto::SourceSpec {
            repo: "https://example.net/team/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            base: None,
        },
        repo: "https://example.net/team/repo.git".to_owned(),
        git_ref: "main".to_owned(),
        base_commit: None,
        task: "add the missing test".to_owned(),
        harness: "opencode".to_owned(),
        provider: Some("peer-a".to_owned()),
        model: None,
        priority: lu_proto::DEFAULT_PRIORITY,
        group: None,
        allow_spawn: false,
        spawn_inherit: false,
        spawn_repos: Vec::new(),
        workspace: None,
        cache_dirs: Vec::new(),
        context: None,
        origin: None,
        origin_ref: None,
        untrusted_context: false,
        draft: false,
        publish: None,
        timeout_secs: 600,
        idle_timeout_secs: Some(60),
        on_stuck: None,
        stuck_after_secs: None,
        idempotency_key: Some("ticket:42".to_owned()),
        after: vec![JobId::new()],
        collect: lu_proto::CollectMode::Apply,
        include_failed: false,
        on_conflict: Default::default(),
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
        allow_empty: false,
        requires: vec!["php8.3".to_owned()],
        tolerates: Vec::new(),
        fallbacks: vec![lu_proto::Choice {
            provider: "peer-b".to_owned(),
            model: Some("other-model".to_owned()),
        }],
        expect: None,
        schedule: None,
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
        stuck_mode: None,
        outcome: Outcome::Failed(FailureClass::Execution),
        exit_code: Some(101),
        summary: Some("harness exited non-zero".to_owned()),
        retry_after_secs: None,
        expect_failed: false,
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
        harness_ok: Some(false),
        disk_free_pct: Some(7),
        jobs: Vec::new(),
        labels: vec!["gpu".to_owned()],
        label_sources: std::collections::BTreeMap::new(),
        taints: Vec::new(),
        workspace_bytes: 0,
        workspace_count: 0,
        workspace_reclaimed_bytes_total: 0,
        workspace_evictions_total: 0,
        workspace_delete_errors_total: 0,
        workspaces: Vec::new(),
    };
    beat.validate().unwrap();
    let wire = serde_json::to_string(&beat).unwrap();
    assert_eq!(serde_json::from_str::<Heartbeat>(&wire).unwrap(), beat);
}

#[test]
fn heartbeat_health_fields_are_optional_and_lenient() {
    let bare = serde_json::json!({"node": "peer-a", "harness": "idle", "future_field": 1});
    let beat: Heartbeat = serde_json::from_value(bare).unwrap();
    assert_eq!(beat.harness_ok, None);
    assert_eq!(beat.disk_free_pct, None);

    let full = serde_json::json!({
        "node": "peer-a", "harness": "idle", "harness_ok": false, "disk_free_pct": 3
    });
    let beat: Heartbeat = serde_json::from_value(full).unwrap();
    assert_eq!(beat.harness_ok, Some(false));
    assert_eq!(beat.disk_free_pct, Some(3));
    let wire = serde_json::to_value(&beat).unwrap();
    assert_eq!(wire["harness_ok"], false);
    assert_eq!(wire["disk_free_pct"], 3);
    let older: Heartbeat = serde_json::from_str(r#"{"node":"peer-a","harness":"idle"}"#).unwrap();
    assert!(older.harness_ok.is_none());

    let bad = serde_json::json!({
        "node": "peer-a", "harness": "idle", "disk_free_pct": 101
    });
    let error = serde_json::from_value::<Heartbeat>(bad).unwrap_err();
    assert!(error.to_string().contains("0 to 100"), "{error}");
}

#[test]
fn node_view_threshold_is_optional_and_drives_health() {
    let full = serde_json::json!({
        "name": "peer-a", "harness_ok": true, "disk_free_pct": 7, "min_disk_free_pct": 8
    });
    let node: NodeView = serde_json::from_value(full).unwrap();
    assert_eq!(node.min_disk_free_pct, Some(8));
    assert_eq!(
        node.health(),
        NodeHealth {
            text: "disk free 7%".to_owned(),
            level: NodeHealthLevel::Critical
        }
    );
    let wire = serde_json::to_value(&node).unwrap();
    assert_eq!(wire["min_disk_free_pct"], 8);

    let older: NodeView = serde_json::from_value(serde_json::json!({
        "name": "peer-a", "harness_ok": false, "disk_free_pct": 7
    }))
    .unwrap();
    assert_eq!(older.min_disk_free_pct, None);
    assert_eq!(
        older.health(),
        NodeHealth {
            text: "harness down".to_owned(),
            level: NodeHealthLevel::Critical
        }
    );

    let warning: NodeView = serde_json::from_value(serde_json::json!({
        "name": "peer-a", "disk_free_pct": 9, "min_disk_free_pct": 8
    }))
    .unwrap();
    assert_eq!(
        warning.health(),
        NodeHealth {
            text: "disk free 9%".to_owned(),
            level: NodeHealthLevel::Warning
        }
    );

    let healthy: NodeView = serde_json::from_value(serde_json::json!({
        "name": "peer-a", "disk_free_pct": 16, "min_disk_free_pct": 8
    }))
    .unwrap();
    assert_eq!(healthy.health(), NodeHealth::healthy());

    let no_threshold: NodeView = serde_json::from_value(serde_json::json!({
        "name": "peer-a", "disk_free_pct": 7
    }))
    .unwrap();
    assert_eq!(no_threshold.min_disk_free_pct, None);
    assert_eq!(no_threshold.health(), NodeHealth::healthy());
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
        schedule: None,
        spec: JobSpec {
            source: lu_proto::SourceSpec {
                repo: "https://example.net/team/repo.git".to_owned(),
                git_ref: "main".to_owned(),
                base: None,
            },
            repo: "https://example.net/team/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            task: "add the missing test".to_owned(),
            provider: Some("peer-a".to_owned()),
            group: Some("nightly".to_owned()),
            allow_spawn: false,
            spawn_inherit: false,
            spawn_repos: Vec::new(),
            workspace: None,
            cache_dirs: Vec::new(),
            context: None,
            origin: None,
            origin_ref: None,
            untrusted_context: false,
            draft: false,
            publish: None,
            timeout_secs: 600,
            idle_timeout_secs: Some(60),
            idempotency_key: Some("ticket:42".to_owned()),
            after: vec![dep.clone()],
            requires: vec!["php8.3".to_owned()],
            fallbacks: vec![Choice {
                provider: "peer-b".to_owned(),
                model: Some("other-model".to_owned()),
            }],
            on_stuck: None,
            stuck_after_secs: None,
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
        waiting_on: vec![dep.clone()],
        parent_job_id: None,
        root_job_id: None,
        depth: 0,
        waiting_reason: None,
        waiting_detail: Vec::new(),
        staged_commit: None,
        blocked_reason: None,
        blocked_until: None,
        outcome_note: None,
        diff_bytes: None,
        scrubbed_hits: 3,
        patch_scrubbed: true,
        patch_secret_hits: lu_proto::view::PatchSecretHitsView {
            count: 1,
            rules: vec!["private_key".to_owned()],
        },
        warnings: Some(Vec::new()),
        audit_write_errors: None,
        publish: None,
        publish_target: None,
    };
    let expected_job = serde_json::json!({
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "state": "running",
        "spec": {
            "source": {"repo": "https://example.net/team/repo.git", "ref": "main"},
            "task": "add the missing test",
            "harness": "opencode",
            "provider": "peer-a",
            "priority": 5,
            "group": "nightly",
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
        "depth": 0,
        "waiting_reason": null,
        "scrubbed_hits": 3u64,
        "patch_scrubbed": true,
        "patch_secret_hits": {"count": 1u64, "rules": ["private_key"]},
        "warnings": []
    });
    assert_eq!(serde_json::to_value(&job).unwrap(), expected_job);

    let node = NodeView {
        name: NodeName::from_str("peer-a").unwrap(),
        version: Some("0.1.10".to_owned()),
        last_seen: 1_700_000_030,
        online: true,
        draining: false,
        drain_origin: None,
        harness: Some(HarnessState::Idle),
        harness_ok: Some(true),
        disk_free_pct: Some(21),
        min_disk_free_pct: Some(8),
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
        cordon_details: vec![
            CordonView {
                provider: "peer-c".to_owned(),
                source: "failure".to_owned(),
                reason: "Provider peer-c failed: timeout".to_owned(),
                set_at: 1_700_000_020,
                until: Some(1_700_000_620),
                lifts_after_healthy_heartbeats: None,
            },
            CordonView {
                provider: "peer-d".to_owned(),
                source: "reiki".to_owned(),
                reason: "Reiki cooldown for provider peer-d (retry after 300s)".to_owned(),
                set_at: 1_700_000_025,
                until: None,
                lifts_after_healthy_heartbeats: Some(2),
            },
        ],
        reiki: Some(serde_json::json!({"status": "ok"})),
        load1: Some(1.5),
        mem_available_kb: Some(8192),
        mem_total_kb: Some(16_384),
        stuck: 1,
        workspace_bytes: 4_096,
        workspace_count: 1,
        workspace_reclaimed_bytes_total: 8_192,
        workspace_evictions_total: 2,
        workspace_delete_errors_total: 1,
        workspaces: vec![WorkspaceView {
            job_key: "ticket:42".to_owned(),
            state: WorkspaceState::Kept,
            bytes: 4_096,
            age_secs: 90,
            policy: WorkspacePolicy::Keep,
        }],
    };
    let expected_node = serde_json::json!({
        "name": "peer-a",
        "version": "0.1.10",
        "last_seen": 1_700_000_030u64,
        "online": true,
        "draining": false,
        "harness": "idle",
        "harness_ok": true,
        "disk_free_pct": 21,
        "min_disk_free_pct": 8,
        "providers": [{"name": "peer-b", "state": "ok"}],
        "provider_models": [{"provider": "peer-b", "model": "other-model"}],
        "labels": ["gpu"],
        "label_sources": {"gpu": "probed"},
        "taints": ["homelab"],
        "current_job": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "cordon_details": [
            {
                "provider": "peer-c",
                "source": "failure",
                "reason": "Provider peer-c failed: timeout",
                "set_at": 1_700_000_020,
                "until": 1_700_000_620,
                "lifts_after_healthy_heartbeats": null
            },
            {
                "provider": "peer-d",
                "source": "reiki",
                "reason": "Reiki cooldown for provider peer-d (retry after 300s)",
                "set_at": 1_700_000_025,
                "until": null,
                "lifts_after_healthy_heartbeats": 2
            }
        ],
        "reiki": {"status": "ok"},
        "load1": 1.5,
        "mem_available_kb": 8192,
        "mem_total_kb": 16_384,
        "stuck": 1,
        "workspace_bytes": 4_096,
        "workspace_count": 1,
        "workspace_reclaimed_bytes_total": 8_192,
        "workspace_evictions_total": 2,
        "workspace_delete_errors_total": 1,
        "workspaces": [{
            "job_key": "ticket:42",
            "state": "kept",
            "bytes": 4_096,
            "age_secs": 90,
            "policy": "keep"
        }]
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

#[test]
fn job_view_echoes_the_expect_block() {
    let view: JobView = serde_json::from_value(serde_json::json!({
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "state": "done",
        "spec": {
            "source": {"repo": "https://example.net/team/repo.git", "ref": "main"},
            "task": "add the missing test",
            "expect": {
                "files": ["REVIEW.md"],
                "changed": ["src/**"],
                "forbid_changed": ["Cargo.lock"],
                "min_diff_bytes": 1,
                "max_diff_bytes": 5242880,
                "mode": "fail"
            }
        }
    }))
    .unwrap();

    let expected = serde_json::json!({
        "files": ["REVIEW.md"],
        "changed": ["src/**"],
        "forbid_changed": ["Cargo.lock"],
        "min_diff_bytes": 1u64,
        "max_diff_bytes": 5242880u64,
        "mode": "fail"
    });
    let value = serde_json::to_value(&view).unwrap();
    assert_eq!(value["spec"]["expect"], expected);
    assert_eq!(
        serde_json::from_value::<JobView>(value)
            .unwrap()
            .spec
            .expect,
        view.spec.expect
    );
}

#[test]
fn schedule_field_round_trips_through_specs_and_views() {
    let schedule = JobSchedule {
        not_before: Some(1_700_000_000),
        deadline: Some(1_700_003_600),
        on_deadline: OnDeadline::Cancel,
        window: Some("nightly".to_owned()),
        ..Default::default()
    };
    let mut spec = spec();
    spec.schedule = Some(schedule.clone());
    spec.validate().unwrap();
    let json = spec.canonical_json();
    assert!(json.contains("\"schedule\":"), "{json}");
    assert_eq!(
        serde_json::from_str::<JobSpec>(&json).unwrap().schedule,
        Some(schedule.clone())
    );

    let mut view = JobView::placeholder(job_id(), JobState::Blocked);
    view.spec = spec;
    view.schedule = Some(schedule);
    view.waiting_detail =
        vec![BlockReason::new(Constraint::Window, "calendar is closed").until(Some(1_700_003_600))];
    let value = serde_json::to_value(&view).unwrap();
    assert_eq!(serde_json::from_value::<JobView>(value).unwrap(), view);
}

#[test]
fn older_job_view_json_without_schedule_fields_still_decodes() {
    let view: JobView = serde_json::from_value(serde_json::json!({
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "state": "queued",
        "spec": {
            "source": {"repo": "https://example.net/team/repo.git", "ref": "main"},
            "task": "add the missing test"
        }
    }))
    .unwrap();
    assert_eq!(view.schedule, None);
    assert!(view.waiting_detail.is_empty());
    let value = serde_json::to_value(&view).unwrap();
    assert!(value.get("schedule").is_none());
    assert!(value.get("waiting_detail").is_none());
}

#[test]
fn schedule_rejects_unknown_fields_on_the_wire() {
    let mut value = serde_json::to_value(spec()).unwrap();
    value["schedule"] = serde_json::json!({"not_before": 1_700_000_000_i64, "surprise": true});
    let error = serde_json::from_value::<JobSpec>(value)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unknown field") || error.contains("surprise"),
        "{error}"
    );
}

#[test]
fn deadline_missed_transitions_are_wired() {
    assert!(JobState::can_transition(
        JobState::Queued,
        JobState::DeadlineMissed
    ));
    assert!(JobState::can_transition(
        JobState::Blocked,
        JobState::DeadlineMissed
    ));
    assert!(!JobState::can_transition(
        JobState::Running,
        JobState::DeadlineMissed
    ));
    assert!(!JobState::can_transition(
        JobState::Expired,
        JobState::DeadlineMissed
    ));
    assert!(JobState::DeadlineMissed.is_terminal());
    assert_eq!(
        "deadline_missed".parse::<JobState>().unwrap(),
        JobState::DeadlineMissed
    );
    assert_eq!(
        serde_json::to_string(&JobState::DeadlineMissed).unwrap(),
        "\"deadline_missed\""
    );
}

#[test]
fn heartbeat_rejects_malformed_disk_free_pct_without_panicking() {
    for bad in [
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!("12"),
        serde_json::json!(null).clone(),
    ] {
        let value = serde_json::json!({"node": "peer-a", "harness": "idle", "disk_free_pct": bad});
        let parsed = serde_json::from_value::<Heartbeat>(value);
        if bad.is_null() {
            assert_eq!(parsed.unwrap().disk_free_pct, None);
        } else {
            assert!(parsed.is_err(), "{bad}");
        }
    }
    let nan = r#"{"node":"peer-a","harness":"idle","disk_free_pct":NaN}"#;
    assert!(serde_json::from_str::<Heartbeat>(nan).is_err());
}
