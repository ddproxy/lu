use super::*;
use crate::conformance::hb;
use lu_proto::HarnessState;

fn name() -> NodeName {
    "peer-a".parse().unwrap()
}

#[test]
fn absent_health_fields_never_block() {
    let beat = hb("peer-a", HarnessState::Idle, vec![]);
    assert_eq!(
        heartbeat_health_reason(&name(), &beat, &DispatchLimits::default()),
        None
    );
}

#[test]
fn bad_harness_or_low_disk_blocks_and_recovers() {
    let limits = DispatchLimits::default();
    let mut beat = hb("peer-a", HarnessState::Idle, vec![]);
    beat.harness_ok = Some(false);
    let reason = heartbeat_health_reason(&name(), &beat, &limits).unwrap();
    assert!(is_node_health_reason(&reason), "{reason}");
    beat.harness_ok = Some(true);
    beat.disk_free_pct = Some(7);
    let reason = heartbeat_health_reason(&name(), &beat, &limits).unwrap();
    assert!(is_node_health_reason(&reason), "{reason}");
    beat.disk_free_pct = Some(8);
    assert_eq!(heartbeat_health_reason(&name(), &beat, &limits), None);
}

#[test]
fn zero_threshold_disables_the_disk_gate() {
    let limits = DispatchLimits {
        min_disk_free_pct: 0,
    };
    let mut beat = hb("peer-a", HarnessState::Idle, vec![]);
    beat.disk_free_pct = Some(0);
    assert_eq!(heartbeat_health_reason(&name(), &beat, &limits), None);
}
