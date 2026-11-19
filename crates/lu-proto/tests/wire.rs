//! End-to-end use of the public API: a job goes out as an assignment, comes back as log chunks
//! and a completion, while a heartbeat reports the node. Nothing here is inside the crate, so it
//! also checks that the re-exports are usable.

use std::str::FromStr;

use lu_proto::{
    printable, truncate_chars, AgentEvent, ArtifactName, Completion, FailureClass, HarnessState,
    Heartbeat, JobId, JobSpec, JobState, LogChunk, NodeName, Outcome, ProviderState,
    ProviderStatus,
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
