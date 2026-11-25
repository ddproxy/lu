//! High-confidence signatures of node-side infrastructure faults.
//!
//! Harness output is untrusted task text: a test may print almost anything. These classifiers
//! therefore match short, specific bootstrap messages, never a bare socket or disk error. Health
//! signals that come from the node's gateway health endpoint are checked separately.

use lu_proto::NodeFailureReason;

/// Recognizes an infrastructure fault in a failed harness's output tail.
pub fn classify_tail(text: &str) -> Option<NodeFailureReason> {
    text.lines().find_map(|line| {
        if harness_unreachable(line) {
            Some(NodeFailureReason::HarnessUnreachable)
        } else if container_start(line) {
            Some(NodeFailureReason::ContainerStart)
        } else if container_wait_timeout(line) {
            Some(NodeFailureReason::ContainerWaitTimeout)
        } else {
            None
        }
    })
}

/// The harness server could not be reached. The phrase must name that server; a bare connection
/// error in task output is not enough.
pub fn harness_unreachable(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    [
        "harness server unreachable",
        "harness server connection failed",
        "could not connect to harness server",
        "connection to harness server refused",
    ]
    .iter()
    .any(|marker| line.starts_with(marker))
}

/// Podman refused to create or start the harness container.
pub fn container_start(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    if line.starts_with("podman:")
        && [
            "cannot start",
            "could not start",
            "failed to start",
            "cannot create",
            "could not create",
            "failed to create",
        ]
        .iter()
        .any(|marker| line.contains(marker))
    {
        return true;
    }
    [
        "error: cannot start container",
        "error: could not start container",
        "container start failed",
        "failed to create container",
        "could not start container",
        "podman create failed",
    ]
    .iter()
    .any(|marker| line.starts_with(marker))
}

/// The agent timed out waiting for Podman's overlay-container storage file.
pub fn container_wait_timeout(line: &str) -> bool {
    line.to_ascii_lowercase()
        .contains("timed out waiting for file /var/lib/containers/storage/overlay-containers/")
}

/// A disk-full error while the node was preparing a job's workspace or checkout.
pub fn workspace_enospc(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let disk_full = lower.contains("no space left on device")
        || lower.contains("enospc")
        || lower.contains("errno 28");
    let preparing = [
        "workspace",
        "staging",
        "stage ",
        "cache dir",
        "checkout",
        "task file",
        "tree",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    disk_full && preparing
}

/// A gateway health response body that matches an operator-configured marker.
///
/// Markers are matched case-insensitively. An empty marker list classifies nothing: a `503` then
/// stays an ordinary failed health read. Empty markers are ignored defensively; config validation
/// rejects them.
pub fn gateway_fault(body: &str, markers: &[String]) -> bool {
    let lower = body.to_lowercase();
    markers
        .iter()
        .filter(|marker| !marker.is_empty())
        .any(|marker| lower.contains(&marker.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_unreachable_needs_an_explicit_server_reference() {
        assert!(harness_unreachable(
            "Harness server unreachable: dial failed"
        ));
        assert!(harness_unreachable(
            "could not connect to harness server at 127.0.0.1:1234"
        ));
        assert!(harness_unreachable("Connection to harness server refused"));
        assert!(!harness_unreachable("connection refused"));
        assert!(!harness_unreachable("the test printed connection refused"));
        assert!(!harness_unreachable("server unreachable"));
    }

    #[test]
    fn container_start_failures_are_recognized() {
        assert!(container_start(
            "podman: failed to start container c: exit status 125"
        ));
        assert!(container_start("Error: cannot start container: no such id"));
        assert!(container_start(
            "container start failed: runtime unavailable"
        ));
        assert!(container_start("podman create failed: image not found"));
        assert!(!container_start("podman version 5.0.0"));
        assert!(!container_start("the task said podman: failed to start"));
        assert!(!container_start("failed to start the harness"));
    }

    #[test]
    fn the_container_wait_timeout_uses_the_full_storage_path() {
        assert!(container_wait_timeout(
            "timed out waiting for file /var/lib/containers/storage/overlay-containers/c123/userdata"
        ));
        assert!(container_wait_timeout(
            "agent: TIMED OUT WAITING FOR FILE /var/lib/containers/storage/overlay-containers/x"
        ));
        assert!(!container_wait_timeout(
            "timed out waiting for file /tmp/work"
        ));
        assert!(!container_wait_timeout(
            "waiting for file /var/lib/containers/storage/overlay-containers/x"
        ));
        assert!(!container_wait_timeout("timed out waiting for container"));
    }

    #[test]
    fn only_infrastructure_lines_classify_a_tail() {
        let tail = "harness ran\n\
                    connection refused\n\
                    podman: could not start container c\n";
        assert_eq!(classify_tail(tail), Some(NodeFailureReason::ContainerStart));
        assert_eq!(
            classify_tail(
                "timed out waiting for file /var/lib/containers/storage/overlay-containers/x"
            ),
            Some(NodeFailureReason::ContainerWaitTimeout)
        );
        assert_eq!(
            classify_tail("harness server unreachable"),
            Some(NodeFailureReason::HarnessUnreachable)
        );
        // Ordinary output that merely mentions a socket is never a node fault.
        assert_eq!(
            classify_tail("curl: connection refused\nassert_eq!(out, \"connection refused\")"),
            None
        );
        assert_eq!(classify_tail(""), None);
    }

    #[test]
    fn enospc_needs_a_preparation_context() {
        assert!(workspace_enospc(
            "staging failed: write /work: no space left on device"
        ));
        assert!(workspace_enospc(
            "workspace cache directories failed: ENOSPC"
        ));
        assert!(workspace_enospc("could not write the task file: errno 28"));
        assert!(!workspace_enospc("no space left on device"));
        assert!(!workspace_enospc("the test reported ENOSPC in its output"));
        assert!(!workspace_enospc("workspace ready"));
    }

    #[test]
    fn gateway_fault_uses_only_configured_markers() {
        let markers = ["Store Offline".to_owned(), "capacity exhausted".to_owned()];
        assert!(gateway_fault("{\"error\":\"STORE OFFLINE\"}", &markers));
        assert!(gateway_fault("shard capacity EXHAUSTED", &markers));
        assert!(!gateway_fault("service unavailable", &markers));
        assert!(!gateway_fault("429 rate limited", &markers));
        assert!(!gateway_fault("", &markers));
    }

    #[test]
    fn an_empty_marker_list_never_matches() {
        assert!(!gateway_fault("store offline", &[]));
        assert!(!gateway_fault("an unmarked gateway failure", &[]));
        assert!(!gateway_fault("", &[]));
    }

    #[test]
    fn empty_markers_are_ignored() {
        assert!(!gateway_fault("store offline", &["".to_owned()]));
    }
}
