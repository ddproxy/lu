#!/bin/sh
# Proof for the named harness profile contract: validation, exact argv, stdin,
# fake-harness success/failure markers, result file, cancel, timeout and stuck.
set -eu

cd "$(dirname "$0")/.."

cargo test --locked -p lu-config --test harness_profiles
cargo test --locked -p lu-supervise --test harness
cargo test --locked -p lu-supervise --test supervise

cargo test --locked -p tuishou-ting --lib profile
cargo test --locked -p tuishou-ting --lib job_happy_path_calls_in_order
cargo test --locked -p tuishou-ting --lib job_failed_exit_completes_execution_failure
cargo test --locked -p tuishou-ting --lib job_provider_marker_completes_provider_failure
cargo test --locked -p tuishou-ting --lib control_loop_cancel_event_stops_job
cargo test --locked -p tuishou-ting --lib stuck_summary_names_a_profile_needs_input_marker
