//! Shared conformance tests for [`crate::Store`] implementations.

use std::collections::HashMap;
use std::str::FromStr;

use lu_proto::{
    ArtifactName, Completion, FailureClass, HarnessState, Heartbeat, JobId, JobSpec, JobState,
    NodeName, Outcome, ProviderState, ProviderStatus, MAX_LOG_BYTES,
};

use crate::{Now, Store, StoreError};

pub fn now(wall_unix: i64, mono_ms: u64) -> Now {
    Now { wall_unix, mono_ms }
}

pub fn spec(repo: &str, key: Option<&str>) -> JobSpec {
    JobSpec {
        repo: repo.to_owned(),
        git_ref: "main".to_owned(),
        task: "do it".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 3600,
        idle_timeout_secs: None,
        idempotency_key: key.map(|s| s.to_owned()),
        after: Vec::new(),
        stack_dependencies: true,
    }
}

pub fn node(name: &str) -> NodeName {
    NodeName::from_str(name).unwrap()
}

pub fn hb(name: &str, harness: HarnessState, providers: Vec<(&str, ProviderState)>) -> Heartbeat {
    Heartbeat {
        node: node(name),
        harness,
        version: None,
        providers: providers
            .into_iter()
            .map(|(n, s)| ProviderStatus {
                name: n.to_owned(),
                state: s,
                soonest_secs: None,
            })
            .collect(),
        reiki: None,
        host: None,
        jobs: Vec::new(),
    }
}

/// Runs the same tests against any store implementation.
pub fn run_all<S: Store>(make: impl Fn() -> S) {
    idempotent_create(&make());
    list_jobs_ordering_and_cursor(&make());
    cancel_states(&make());
    cancel_recovering_ends_it_cancelled_immediately(&make());
    assignment_and_epoch(&make());
    start_renew_log_artifact(&make());
    complete_and_retry_cordon(&make());
    conflict_failure_class_round_trips(&make());
    stale_epoch_rejected(&make());
    expire_and_recover(&make());
    recover_on_boot(&make());
    recovery_grace_survives_a_young_clock(&make());
    a_recovering_job_resumes_or_finishes_for_its_holder(&make());
    nodes_view(&make());
    drain_unknown_node_is_rejected(&make());
    blocked_on_submit_and_unblocked_on_done(&make());
    dependencies_already_done_queue_immediately(&make());
    failed_dependency_cascades_two_levels(&make());
    cancelled_dependency_cascades_two_levels(&make());
    diamond_dependency(&make());
    unknown_and_invalid_dependencies_are_rejected(&make());
    job_counts(&make());
}

fn with_after(mut sp: JobSpec, after: &[JobId]) -> JobSpec {
    sp.after = after.to_vec();
    sp
}

fn finish_done<S: Store>(s: &S, id: &JobId, node_name: &str, wall: i64, mono: u64) {
    s.heartbeat(&hb(node_name, HarnessState::Idle, vec![]), now(wall, mono))
        .unwrap();
    let n = node(node_name);
    let a = s
        .next_assignment(&n, now(wall, mono + 1), 60_000)
        .unwrap()
        .unwrap();
    assert_eq!(a.job_id, *id);
    s.start_job(id, a.epoch, now(wall, mono + 2)).unwrap();
    let comp = Completion {
        job_id: id.clone(),
        epoch: a.epoch,
        outcome: Outcome::Done,
        exit_code: Some(0),
        summary: None,
        retry_after_secs: None,
    };
    s.complete(&comp, None, now(wall, mono + 3), 1, 1000)
        .unwrap();
}

fn finish_failed<S: Store>(s: &S, id: &JobId, node_name: &str, wall: i64, mono: u64) {
    s.heartbeat(&hb(node_name, HarnessState::Idle, vec![]), now(wall, mono))
        .unwrap();
    let n = node(node_name);
    let a = s
        .next_assignment(&n, now(wall, mono + 1), 60_000)
        .unwrap()
        .unwrap();
    assert_eq!(a.job_id, *id);
    s.start_job(id, a.epoch, now(wall, mono + 2)).unwrap();
    let comp = Completion {
        job_id: id.clone(),
        epoch: a.epoch,
        outcome: Outcome::Failed(FailureClass::Execution),
        exit_code: Some(1),
        summary: Some("boom".into()),
        retry_after_secs: None,
    };
    s.complete(
        &comp,
        Some((FailureClass::Execution, None)),
        now(wall, mono + 3),
        1,
        1000,
    )
    .unwrap();
}

fn conflict_failure_class_round_trips<S: Store>(s: &S) {
    let (job, _) = s
        .create_job(&spec("https://x/conflict.git", None), now(1, 1))
        .unwrap();
    s.heartbeat(&hb("conflict-node", HarnessState::Idle, vec![]), now(2, 2))
        .unwrap();
    let a = s
        .next_assignment(&node("conflict-node"), now(3, 3), 60_000)
        .unwrap()
        .unwrap();
    s.start_job(&job.id, a.epoch, now(4, 4)).unwrap();
    let comp = Completion {
        job_id: job.id.clone(),
        epoch: a.epoch,
        outcome: Outcome::Failed(FailureClass::Conflict),
        exit_code: None,
        summary: Some("dependency dep-1 conflict in a.txt".into()),
        retry_after_secs: None,
    };
    s.complete(
        &comp,
        Some((FailureClass::Conflict, None)),
        now(5, 5),
        1,
        1000,
    )
    .unwrap();
    let rec = s.get_job(&job.id).unwrap();
    assert_eq!(rec.state, JobState::Failed);
    assert_eq!(rec.failure_class, Some(FailureClass::Conflict));
    assert_eq!(
        rec.summary.as_deref(),
        Some("dependency dep-1 conflict in a.txt")
    );
}

fn blocked_on_submit_and_unblocked_on_done<S: Store>(s: &S) {
    let (a, _) = s
        .create_job(&spec("https://x/a.git", None), now(1, 1))
        .unwrap();
    let (b, _) = s
        .create_job(
            &with_after(spec("https://x/b.git", None), std::slice::from_ref(&a.id)),
            now(2, 2),
        )
        .unwrap();
    assert_eq!(b.state, JobState::Blocked);
    assert_eq!(b.waiting_on, vec![a.id.clone()]);
    assert_eq!(
        s.list_jobs(Some(JobState::Blocked), 10, None).unwrap()[0].id,
        b.id
    );

    finish_done(s, &a.id, "dep-node-a", 10, 10);
    let b2 = s.get_job(&b.id).unwrap();
    assert_eq!(b2.state, JobState::Queued);
    assert!(b2.waiting_on.is_empty());

    let (c, _) = s
        .create_job(
            &with_after(spec("https://x/c.git", None), std::slice::from_ref(&b.id)),
            now(3, 3),
        )
        .unwrap();
    let cancelled = s.cancel_job(&c.id, now(4, 4)).unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
}

fn dependencies_already_done_queue_immediately<S: Store>(s: &S) {
    let (a, _) = s
        .create_job(&spec("https://x/a.git", None), now(1, 1))
        .unwrap();
    finish_done(s, &a.id, "done-node-a", 10, 10);
    let (b, _) = s
        .create_job(
            &with_after(spec("https://x/b.git", None), std::slice::from_ref(&a.id)),
            now(2, 2),
        )
        .unwrap();
    assert_eq!(b.state, JobState::Queued);
    assert!(b.waiting_on.is_empty());
}

fn failed_dependency_cascades_two_levels<S: Store>(s: &S) {
    let (a, _) = s
        .create_job(&spec("https://x/a.git", None), now(1, 1))
        .unwrap();
    let (b, _) = s
        .create_job(
            &with_after(spec("https://x/b.git", None), std::slice::from_ref(&a.id)),
            now(2, 2),
        )
        .unwrap();
    let (c, _) = s
        .create_job(
            &with_after(spec("https://x/c.git", None), std::slice::from_ref(&b.id)),
            now(3, 3),
        )
        .unwrap();
    finish_failed(s, &a.id, "fail-node-a", 10, 10);

    let b2 = s.get_job(&b.id).unwrap();
    assert_eq!(b2.state, JobState::Failed);
    assert_eq!(b2.failure_class, Some(FailureClass::Dependency));
    assert_eq!(
        b2.summary.as_deref(),
        Some(format!("dependency {} failed", a.id).as_str())
    );
    let c2 = s.get_job(&c.id).unwrap();
    assert_eq!(c2.state, JobState::Failed);
    assert_eq!(c2.failure_class, Some(FailureClass::Dependency));
    assert!(c2
        .summary
        .as_deref()
        .is_some_and(|m| m.starts_with(&format!("dependency {} ", b.id))));
}

fn cancelled_dependency_cascades_two_levels<S: Store>(s: &S) {
    let (a, _) = s
        .create_job(&spec("https://x/a.git", None), now(1, 1))
        .unwrap();
    let (b, _) = s
        .create_job(
            &with_after(spec("https://x/b.git", None), std::slice::from_ref(&a.id)),
            now(2, 2),
        )
        .unwrap();
    let (c, _) = s
        .create_job(
            &with_after(spec("https://x/c.git", None), std::slice::from_ref(&b.id)),
            now(3, 3),
        )
        .unwrap();
    s.cancel_job(&a.id, now(4, 4)).unwrap();
    let b2 = s.get_job(&b.id).unwrap();
    assert_eq!(b2.state, JobState::Failed);
    assert_eq!(b2.failure_class, Some(FailureClass::Dependency));
    assert_eq!(
        b2.summary.as_deref(),
        Some(format!("dependency {} cancelled", a.id).as_str())
    );
    let c2 = s.get_job(&c.id).unwrap();
    assert_eq!(c2.state, JobState::Failed);
}

fn diamond_dependency<S: Store>(s: &S) {
    let (a, _) = s
        .create_job(&spec("https://x/a.git", None), now(1, 1))
        .unwrap();
    let (b, _) = s
        .create_job(
            &with_after(spec("https://x/b.git", None), std::slice::from_ref(&a.id)),
            now(2, 2),
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let (c, _) = s
        .create_job(
            &with_after(spec("https://x/c.git", None), std::slice::from_ref(&a.id)),
            now(3, 3),
        )
        .unwrap();
    let (d, _) = s
        .create_job(
            &with_after(spec("https://x/d.git", None), &[b.id.clone(), c.id.clone()]),
            now(4, 4),
        )
        .unwrap();
    finish_done(s, &a.id, "diamond-a", 10, 10);
    finish_done(s, &b.id, "diamond-b", 20, 20);
    let d2 = s.get_job(&d.id).unwrap();
    assert_eq!(d2.state, JobState::Blocked);
    assert_eq!(d2.waiting_on, vec![c.id.clone()]);
    finish_done(s, &c.id, "diamond-c", 30, 30);
    let d3 = s.get_job(&d.id).unwrap();
    assert_eq!(d3.state, JobState::Queued);
    assert!(d3.waiting_on.is_empty());
    finish_done(s, &d.id, "diamond-d", 35, 35);

    let (e, _) = s
        .create_job(&spec("https://x/e.git", None), now(5, 5))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let (f, _) = s
        .create_job(&spec("https://x/f.git", None), now(6, 6))
        .unwrap();
    let (g, _) = s
        .create_job(
            &with_after(spec("https://x/g.git", None), &[e.id.clone(), f.id.clone()]),
            now(7, 7),
        )
        .unwrap();
    finish_failed(s, &e.id, "diamond-e", 40, 40);
    let g2 = s.get_job(&g.id).unwrap();
    assert_eq!(g2.state, JobState::Failed);
    assert_eq!(g2.failure_class, Some(FailureClass::Dependency));
    assert_eq!(
        s.get_job(&f.id).unwrap().state,
        JobState::Queued,
        "an unrelated branch is untouched"
    );
}

fn unknown_and_invalid_dependencies_are_rejected<S: Store>(s: &S) {
    let missing = JobId::new();
    let err = s
        .create_job(
            &with_after(
                spec("https://x/u.git", None),
                std::slice::from_ref(&missing),
            ),
            now(1, 1),
        )
        .unwrap_err();
    assert_eq!(err, StoreError::InvalidDependency(missing));

    let (dead, _) = s
        .create_job(&spec("https://x/dead.git", None), now(1, 1))
        .unwrap();
    s.cancel_job(&dead.id, now(2, 2)).unwrap();
    let err = s
        .create_job(
            &with_after(
                spec("https://x/late.git", None),
                std::slice::from_ref(&dead.id),
            ),
            now(3, 3),
        )
        .unwrap_err();
    assert_eq!(err, StoreError::DeadDependency(dead.id));

    let too_many: Vec<JobId> = (0..=lu_proto::MAX_AFTER_JOBS)
        .map(|_| JobId::new())
        .collect();
    let err = s
        .create_job(
            &with_after(spec("https://x/m.git", None), &too_many),
            now(2, 2),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSpec(_)));

    let dup = JobId::new();
    let err = s
        .create_job(
            &with_after(spec("https://x/d.git", None), &[dup.clone(), dup]),
            now(3, 3),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSpec(_)));
}

fn idempotent_create<S: Store>(s: &S) {
    let n = now(100, 1000);
    let a = spec("https://example.net/a.git", Some("k1"));
    let (j1, created1) = s.create_job(&a, n).unwrap();
    assert!(created1);
    assert_eq!(j1.attempt, 1);
    assert_eq!(j1.state, JobState::Queued);
    // ULID-like id from lu-proto
    assert_eq!(j1.id.as_str().len(), 26);
    JobId::from_str(j1.id.as_str()).unwrap();

    let (j2, created2) = s.create_job(&a, now(101, 1001)).unwrap();
    assert!(!created2);
    assert_eq!(j1.id, j2.id);

    let b = spec("https://example.net/b.git", Some("k1"));
    let err = s.create_job(&b, n).unwrap_err();
    assert!(matches!(err, StoreError::Conflict { length: None }));

    // no key => always new
    let c = spec("https://example.net/c.git", None);
    let (j3, _) = s.create_job(&c, n).unwrap();
    let (j4, _) = s.create_job(&c, n).unwrap();
    assert_ne!(j3.id, j4.id);
}

fn list_jobs_ordering_and_cursor<S: Store>(s: &S) {
    let mut ids = Vec::new();
    for i in 0..3 {
        let (j, _) = s
            .create_job(
                &spec(&format!("https://x/{i}.git"), None),
                now(100 + i, 1000 + i as u64),
            )
            .unwrap();
        ids.push(j.id);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let all = s.list_jobs(None, 10, None).unwrap();
    assert_eq!(all.len(), 3);
    // newest first
    assert!(all[0].id > all[1].id);
    assert!(all[1].id > all[2].id);
    let page = s.list_jobs(None, 1, None).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, all[0].id);
    let after = s.list_jobs(None, 10, Some(&all[0].id)).unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].id, all[1].id);
    // filter by state
    let queued = s.list_jobs(Some(JobState::Queued), 10, None).unwrap();
    assert_eq!(queued.len(), 3);
    let done = s.list_jobs(Some(JobState::Done), 10, None).unwrap();
    assert!(done.is_empty());
}

fn cancel_states<S: Store>(s: &S) {
    let (q, _) = s
        .create_job(&spec("https://x/q.git", None), now(1, 1))
        .unwrap();
    let cancelled = s.cancel_job(&q.id, now(2, 2)).unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert!(s.cancel_job(&q.id, now(3, 3)).is_err());

    let n = node("node-a");
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let (j, _) = s
        .create_job(&spec("https://x/r.git", None), now(1, 1))
        .unwrap();
    let a = s.next_assignment(&n, now(2, 2), 60_000).unwrap().unwrap();
    assert_eq!(a.job_id, j.id);
    let c = s.cancel_job(&j.id, now(3, 3)).unwrap();
    assert!(c.cancel_requested);
    assert_eq!(c.state, JobState::Leased);
    s.start_job(&j.id, a.epoch, now(4, 4)).unwrap();
    let c2 = s.cancel_job(&j.id, now(5, 5)).unwrap();
    assert!(c2.cancel_requested);
    assert_eq!(c2.state, JobState::Running);
}

fn cancel_recovering_ends_it_cancelled_immediately<S: Store>(s: &S) {
    let (dep, _) = s
        .create_job(&spec("https://x/dep.git", None), now(1, 1))
        .unwrap();
    let dependent = {
        let mut sp = spec("https://x/dependent.git", None);
        sp.after = vec![dep.id.clone()];
        s.create_job(&sp, now(1, 1)).unwrap().0
    };
    let node = node("node-a");
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let a = s
        .next_assignment(&node, now(2, 2), 60_000)
        .unwrap()
        .unwrap();
    assert_eq!(a.job_id, dep.id);
    s.start_job(&dep.id, a.epoch, now(3, 3)).unwrap();
    assert_eq!(s.recover_on_boot(now(4, 4)).unwrap(), 1);
    assert_eq!(s.get_job(&dep.id).unwrap().state, JobState::Recovering);

    let cancelled = s.cancel_job(&dep.id, now(5, 5)).unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert_eq!(cancelled.finished_at, Some(5));
    assert_eq!(cancelled.updated_at, 5);
    // the lease and the recovering marker are released: nothing requeues or renews it
    assert!(s
        .requeue_recovering(now(60_000, 60_000), 1, 10)
        .unwrap()
        .is_empty());
    assert!(s.renew_lease(&dep.id, a.epoch, now(6, 6), 60_000).is_err());
    // its blocked dependent fails the way a cancelled dependency does
    let failed = s.get_job(&dependent.id).unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.failure_class, Some(FailureClass::Dependency));
    assert_eq!(
        failed.summary.as_deref(),
        Some(format!("dependency {} cancelled", dep.id).as_str())
    );
    // terminal: a second cancel conflicts
    assert_eq!(
        s.cancel_job(&dep.id, now(6, 6)).unwrap_err(),
        StoreError::Conflict { length: None }
    );
}

fn assignment_and_epoch<S: Store>(s: &S) {
    let n1 = node("node-1");
    let n2 = node("node-2");
    s.heartbeat(&hb("node-1", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    s.heartbeat(&hb("node-2", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let (j1, _) = s
        .create_job(&spec("https://x/1.git", None), now(1, 1))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let (j2, _) = s
        .create_job(&spec("https://x/2.git", None), now(2, 2))
        .unwrap();
    // oldest queued first
    let a1 = s.next_assignment(&n1, now(3, 3), 60_000).unwrap().unwrap();
    assert_eq!(a1.job_id, j1.id);
    assert_eq!(a1.epoch, 1);
    // no job held by n1 -> none (n1 holds j1)
    assert!(s.next_assignment(&n1, now(4, 4), 60_000).unwrap().is_none());
    // n2 gets j2, never same job twice
    let a2 = s.next_assignment(&n2, now(5, 5), 60_000).unwrap().unwrap();
    assert_eq!(a2.job_id, j2.id);
    assert_ne!(a1.job_id, a2.job_id);

    // draining node gets nothing
    let (j3, _) = s
        .create_job(&spec("https://x/3.git", None), now(6, 6))
        .unwrap();
    let nd = node("drain");
    s.heartbeat(&hb("drain", HarnessState::Idle, vec![]), now(6, 6))
        .unwrap();
    s.set_draining(&nd, true).unwrap();
    assert!(s.next_assignment(&nd, now(7, 7), 60_000).unwrap().is_none());
    s.set_draining(&nd, false).unwrap();
    let a3 = s.next_assignment(&nd, now(8, 8), 60_000).unwrap().unwrap();
    assert_eq!(a3.job_id, j3.id);

    // provider constraints
    let mut sp = spec("https://x/p.git", None);
    sp.provider = Some("prov-a".to_owned());
    let (jp, _) = s.create_job(&sp, now(9, 9)).unwrap();
    let np = node("np");
    s.heartbeat(
        &hb(
            "np",
            HarnessState::Idle,
            vec![("prov-b", ProviderState::Ok)],
        ),
        now(9, 9),
    )
    .unwrap();
    assert!(s
        .next_assignment(&np, now(10, 10), 60_000)
        .unwrap()
        .is_none());
    s.heartbeat(
        &hb(
            "np",
            HarnessState::Idle,
            vec![("prov-a", ProviderState::Ok)],
        ),
        now(11, 11),
    )
    .unwrap();
    let ap = s
        .next_assignment(&np, now(12, 12), 60_000)
        .unwrap()
        .unwrap();
    assert_eq!(ap.job_id, jp.id);
}

fn start_renew_log_artifact<S: Store>(s: &S) {
    let n = node("node-a");
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let (j, _) = s
        .create_job(&spec("https://x/l.git", None), now(1, 1))
        .unwrap();
    let a = s.next_assignment(&n, now(2, 2), 60_000).unwrap().unwrap();
    let running = s.start_job(&j.id, a.epoch, now(3, 3)).unwrap();
    assert_eq!(running.state, JobState::Running);
    let cancel = s.renew_lease(&j.id, a.epoch, now(4, 4), 60_000).unwrap();
    assert!(!cancel);

    // log offsets
    let len = s.reserve_log(&j.id, a.epoch, 0, 5).unwrap();
    assert_eq!(len, 5);
    let err = s.reserve_log(&j.id, a.epoch, 0, 1).unwrap_err();
    assert_eq!(err, StoreError::Conflict { length: Some(5) });
    let len2 = s.reserve_log(&j.id, a.epoch, 5, 3).unwrap();
    assert_eq!(len2, 8);

    // log cap behavior
    let cap = MAX_LOG_BYTES;
    let len3 = s.reserve_log(&j.id, a.epoch, 8, cap + 100).unwrap();
    assert_eq!(len3, cap);
    let len4 = s.reserve_log(&j.id, a.epoch, cap, 10).unwrap();
    assert_eq!(len4, cap);

    s.record_artifact(&j.id, a.epoch, ArtifactName::DiffPatch)
        .unwrap();
    let rec = s.get_job(&j.id).unwrap();
    assert!(rec.artifacts.contains(&ArtifactName::DiffPatch));
}

fn complete_and_retry_cordon<S: Store>(s: &S) {
    let n = node("node-a");
    s.heartbeat(
        &hb(
            "node-a",
            HarnessState::Idle,
            vec![("p1", ProviderState::Ok)],
        ),
        now(1, 1),
    )
    .unwrap();
    let mut sp = spec("https://x/c.git", None);
    sp.provider = Some("p1".to_owned());
    let (j, _) = s.create_job(&sp, now(1, 1)).unwrap();
    let a = s.next_assignment(&n, now(2, 2), 60_000).unwrap().unwrap();
    s.start_job(&j.id, a.epoch, now(3, 3)).unwrap();
    // provider failure with retry -> queued attempt+1 and cordon
    let comp = Completion {
        job_id: j.id.clone(),
        epoch: a.epoch,
        outcome: Outcome::Failed(FailureClass::Provider),
        exit_code: None,
        summary: Some("nope".into()),
        retry_after_secs: None,
    };
    let rec = s
        .complete(
            &comp,
            Some((FailureClass::Provider, Some("p1".into()))),
            now(4, 4),
            1,
            10_000,
        )
        .unwrap();
    assert_eq!(rec.state, JobState::Queued);
    assert_eq!(rec.attempt, 2);
    assert_eq!(rec.failure_class, Some(FailureClass::Provider));
    // cordoned so cannot assign
    assert!(s.next_assignment(&n, now(5, 5), 60_000).unwrap().is_none());
    // after expiry
    assert!(s
        .next_assignment(&n, now(20_000, 20_000), 60_000)
        .unwrap()
        .is_some());

    // execution failure never retries
    let (j2, _) = s
        .create_job(&spec("https://x/e.git", None), now(1, 1))
        .unwrap();
    let n2 = node("node-b");
    s.heartbeat(&hb("node-b", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let a2 = s.next_assignment(&n2, now(2, 2), 60_000).unwrap().unwrap();
    assert_eq!(a2.job_id, j2.id);
    let comp2 = Completion {
        job_id: j2.id.clone(),
        epoch: a2.epoch,
        outcome: Outcome::Failed(FailureClass::Execution),
        exit_code: Some(1),
        summary: None,
        retry_after_secs: None,
    };
    let rec2 = s
        .complete(
            &comp2,
            Some((FailureClass::Execution, None)),
            now(3, 3),
            1,
            1000,
        )
        .unwrap();
    assert_eq!(rec2.state, JobState::Failed);

    // success
    let (j3, _) = s
        .create_job(&spec("https://x/s.git", None), now(1, 1))
        .unwrap();
    let n3 = node("node-c");
    s.heartbeat(&hb("node-c", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let a3 = s.next_assignment(&n3, now(2, 2), 60_000).unwrap().unwrap();
    s.start_job(&j3.id, a3.epoch, now(3, 3)).unwrap();
    let comp3 = Completion {
        job_id: j3.id.clone(),
        epoch: a3.epoch,
        outcome: Outcome::Done,
        exit_code: Some(0),
        summary: None,
        retry_after_secs: None,
    };
    let rec3 = s.complete(&comp3, None, now(4, 4), 1, 1000).unwrap();
    assert_eq!(rec3.state, JobState::Done);
}

fn stale_epoch_rejected<S: Store>(s: &S) {
    let n = node("node-a");
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let (j, _) = s
        .create_job(&spec("https://x/stale.git", None), now(1, 1))
        .unwrap();
    let a = s.next_assignment(&n, now(2, 2), 60_000).unwrap().unwrap();
    assert_eq!(
        s.start_job(&j.id, a.epoch - 1, now(3, 3)).unwrap_err(),
        StoreError::StaleEpoch
    );
    assert_eq!(
        s.renew_lease(&j.id, a.epoch - 1, now(3, 3), 60_000)
            .unwrap_err(),
        StoreError::StaleEpoch
    );
    assert_eq!(
        s.reserve_log(&j.id, a.epoch - 1, 0, 1).unwrap_err(),
        StoreError::StaleEpoch
    );
    assert_eq!(
        s.record_artifact(&j.id, a.epoch - 1, ArtifactName::RunLog)
            .unwrap_err(),
        StoreError::StaleEpoch
    );
    let comp = Completion {
        job_id: j.id.clone(),
        epoch: a.epoch - 1,
        outcome: Outcome::Done,
        exit_code: Some(0),
        summary: None,
        retry_after_secs: None,
    };
    assert_eq!(
        s.complete(&comp, None, now(3, 3), 1, 1000).unwrap_err(),
        StoreError::StaleEpoch
    );
    let unchanged = s.get_job(&j.id).unwrap();
    assert_eq!(unchanged.state, JobState::Leased);
}

fn expire_and_recover<S: Store>(s: &S) {
    let n = node("node-a");
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let (j, _) = s
        .create_job(&spec("https://x/exp.git", None), now(1, 1))
        .unwrap();
    s.next_assignment(&n, now(2, 2), 100).unwrap().unwrap();
    let moved = s.expire_leases(now(200, 200)).unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0], j.id);
    assert_eq!(s.get_job(&j.id).unwrap().state, JobState::Recovering);

    // recovering -> queued after grace (retry)
    let moved2 = s.requeue_recovering(now(400, 400), 100, 1).unwrap();
    assert_eq!(moved2.len(), 1);
    let rec = s.get_job(&j.id).unwrap();
    assert_eq!(rec.state, JobState::Queued);
    assert_eq!(rec.attempt, 2);
    assert_eq!(rec.failure_class, None);

    // exhausted attempts -> failed
    let nb = node("node-b");
    s.heartbeat(&hb("node-b", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let a = s.next_assignment(&nb, now(500, 500), 100).unwrap().unwrap();
    assert_eq!(a.job_id, j.id);
    s.expire_leases(now(700, 700)).unwrap();
    let moved3 = s.requeue_recovering(now(900, 900), 100, 1).unwrap();
    assert_eq!(moved3.len(), 1);
    let rec2 = s.get_job(&j.id).unwrap();
    assert_eq!(rec2.state, JobState::Failed);
    assert_eq!(rec2.failure_class, Some(FailureClass::Node));
}

fn recover_on_boot<S: Store>(s: &S) {
    let n = node("node-a");
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    let (j, _) = s
        .create_job(&spec("https://x/boot.git", None), now(1, 1))
        .unwrap();
    s.next_assignment(&n, now(2, 2), 60_000).unwrap().unwrap();
    let c = s.recover_on_boot(now(3, 3)).unwrap();
    assert_eq!(c, 1);
    assert_eq!(s.get_job(&j.id).unwrap().state, JobState::Recovering);
}

fn drain_unknown_node_is_rejected<S: Store>(s: &S) {
    let ghost = node("ghost");
    // A node that has never sent a heartbeat is unknown; draining it must not persist anything.
    assert_eq!(
        s.set_draining(&ghost, true).unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        s.set_draining(&ghost, false).unwrap_err(),
        StoreError::NotFound
    );
    s.heartbeat(&hb("ghost", HarnessState::Idle, vec![]), now(1, 1))
        .unwrap();
    s.set_draining(&ghost, true).unwrap();
    let view = s.nodes(now(2, 2), 1000).unwrap();
    assert!(
        view.iter()
            .find(|n| n.name.as_str() == "ghost")
            .unwrap()
            .draining,
        "a known node drains"
    );
    s.set_draining(&ghost, false).unwrap();
    let view = s.nodes(now(3, 3), 1000).unwrap();
    assert!(
        !view
            .iter()
            .find(|n| n.name.as_str() == "ghost")
            .unwrap()
            .draining,
        "a known node undrains"
    );
}

fn nodes_view<S: Store>(s: &S) {
    let n = node("node-a");
    s.heartbeat(
        &hb(
            "node-a",
            HarnessState::Idle,
            vec![("p1", ProviderState::Ok)],
        ),
        now(10, 1000),
    )
    .unwrap();
    s.set_draining(&n, true).unwrap();
    let view = s.nodes(now(20, 1500), 1000).unwrap();
    assert_eq!(view.len(), 1);
    let v = &view[0];
    assert_eq!(v.name.as_str(), "node-a");
    assert!(v.draining);
    assert!(v.online);
    // not online if outside 2*lease_ms
    let view2 = s.nodes(now(20, 5000), 1000).unwrap();
    assert!(!view2[0].online);

    // cordons shown
    let (j, _) = s
        .create_job(&spec("https://x/c2.git", None), now(1, 1))
        .unwrap();
    let _ = j;
    // create cordon via provider failure
}

/// A job that only just began recovering must wait out the grace even when the clock is near zero.
fn recovery_grace_survives_a_young_clock<S: Store>(s: &S) {
    let (rec, _) = s
        .create_job(&spec("https://example.net/r.git", None), now(0, 0))
        .unwrap();
    let node = NodeName::from_str("node-a").unwrap();
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(0, 0))
        .unwrap();
    let a = s.next_assignment(&node, now(1, 1), 1000).unwrap().unwrap();
    s.start_job(&a.job_id, a.epoch, now(2, 2)).unwrap();
    assert_eq!(s.recover_on_boot(now(3, 3)).unwrap(), 1);
    let moved = s.requeue_recovering(now(3, 3), 30_000, 1).unwrap();
    assert!(moved.is_empty(), "inside the grace nothing is requeued");
    let moved = s
        .requeue_recovering(now(31_000, 31_000), 30_000, 1)
        .unwrap();
    assert_eq!(moved, vec![rec.id]);
}

/// A controller restart must not strand an agent that kept its lease.
fn a_recovering_job_resumes_or_finishes_for_its_holder<S: Store>(s: &S) {
    let node = NodeName::from_str("node-a").unwrap();
    s.heartbeat(&hb("node-a", HarnessState::Idle, vec![]), now(0, 0))
        .unwrap();
    // Renew resumes.
    s.create_job(&spec("https://example.net/r.git", Some("one")), now(0, 0))
        .unwrap();
    let a = s.next_assignment(&node, now(1, 1), 1000).unwrap().unwrap();
    s.start_job(&a.job_id, a.epoch, now(2, 2)).unwrap();
    s.recover_on_boot(now(3, 3)).unwrap();
    assert_eq!(s.get_job(&a.job_id).unwrap().state, JobState::Recovering);
    s.reserve_log(&a.job_id, a.epoch, 0, 4).unwrap();
    s.renew_lease(&a.job_id, a.epoch, now(4, 4), 1000).unwrap();
    assert_eq!(s.get_job(&a.job_id).unwrap().state, JobState::Running);
    assert!(s
        .requeue_recovering(now(90_000, 90_000), 30_000, 1)
        .unwrap()
        .is_empty());
    // A stale epoch still cannot resume.
    s.recover_on_boot(now(5, 5)).unwrap();
    assert_eq!(
        s.renew_lease(&a.job_id, a.epoch + 1, now(6, 6), 1000),
        Err(StoreError::StaleEpoch)
    );
}

fn job_counts<S: Store>(s: &S) {
    let n = node("node-a");
    s.heartbeat(
        &hb(
            "node-a",
            HarnessState::Idle,
            vec![("p1", ProviderState::Ok)],
        ),
        now(10, 1000),
    )
    .unwrap();
    s.set_draining(&n, true).unwrap();
    // no jobs yet
    let c = s.job_counts(now(20, 1500), 1000).unwrap();
    assert_eq!(c.nodes, 1);
    assert_eq!(c.nodes_online, 1);
    assert_eq!(c.nodes_drained, 1);
    // job counts zero for all states
    for (st, cnt) in &c.jobs {
        assert_eq!(*cnt, 0, "{st:?} should be zero");
    }
    assert_eq!(c.completed_done, 0);
    assert_eq!(c.completed_failed, 0);
    assert_eq!(c.completed_cancelled, 0);

    // create some jobs
    let (j1, _) = s
        .create_job(&spec("https://x/1.git", None), now(1, 1))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let (j2, _) = s
        .create_job(&spec("https://x/2.git", None), now(2, 2))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let (j3, _) = s
        .create_job(&spec("https://x/3.git", None), now(3, 3))
        .unwrap();
    // undrain for assignment tests
    s.set_draining(&n, false).unwrap();

    let a1 = s.next_assignment(&n, now(4, 4), 60_000).unwrap().unwrap();
    assert_eq!(a1.job_id, j1.id);
    s.start_job(&j1.id, a1.epoch, now(5, 5)).unwrap();
    // complete j1 done first so the node is free for the next assignment
    let comp1 = Completion {
        job_id: j1.id.clone(),
        epoch: a1.epoch,
        outcome: Outcome::Done,
        exit_code: Some(0),
        summary: None,
        retry_after_secs: None,
    };
    s.complete(&comp1, None, now(7, 7), 1, 1000).unwrap();

    let a2 = s.next_assignment(&n, now(8, 8), 60_000).unwrap().unwrap();
    assert_eq!(a2.job_id, j2.id);
    s.start_job(&j2.id, a2.epoch, now(9, 9)).unwrap();
    // complete j2 failed
    let comp2 = Completion {
        job_id: j2.id.clone(),
        epoch: a2.epoch,
        outcome: Outcome::Failed(FailureClass::Execution),
        exit_code: Some(1),
        summary: None,
        retry_after_secs: None,
    };
    s.complete(
        &comp2,
        Some((FailureClass::Execution, None)),
        now(10, 10),
        1,
        1000,
    )
    .unwrap();
    // cancel j3 queued
    s.cancel_job(&j3.id, now(11, 11)).unwrap();

    let c = s.job_counts(now(20, 1500), 1000).unwrap();
    // j1 done, j2 failed, j3 cancelled
    assert_eq!(c.completed_done, 1);
    assert_eq!(c.completed_failed, 1);
    assert_eq!(c.completed_cancelled, 1);
    // states
    let states: HashMap<_, _> = c.jobs.into_iter().collect();
    assert_eq!(states[&JobState::Done], 1);
    assert_eq!(states[&JobState::Failed], 1);
    assert_eq!(states[&JobState::Cancelled], 1);
    // no queued/leased/running/recovering/needs_input
    assert_eq!(states[&JobState::Queued], 0);
    assert_eq!(states[&JobState::Leased], 0);
    assert_eq!(states[&JobState::Running], 0);
    assert_eq!(states[&JobState::Recovering], 0);
    assert_eq!(states[&JobState::NeedsInput], 0);
}
