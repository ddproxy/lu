//! Publish status stored with a job and shown in operator views.
//!
//! The push credential never appears here: only the remote name, branch, commit and gate results
//! are persisted. A gate result is only trusted when the child job has a recorded exit status.

use serde::{Deserialize, Serialize};

use crate::PublishTarget;

/// Gate outcomes recorded for one publish attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishGateRecord {
    pub name: String,
    pub job_id: String,
    /// `pass`, `fail`, `error` or `pending`.
    pub result: String,
    pub exit_code: Option<i32>,
}

/// Everything the controller knows about a publish attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRecord {
    /// `gates_running`, `gates_passed`, `gates_failed`, `hold`, `pushing`, `published`,
    /// `published_snapshot_fallback`, `no_snapshot_prefix` or `failed`.
    pub detail: String,
    pub branch: String,
    pub remote: String,
    pub base_commit: String,
    pub commit: Option<String>,
    #[serde(default)]
    pub gates: Vec<PublishGateRecord>,
    /// Effective target after redirects and opt-ins: `snapshot` or `source_branch`.
    #[serde(default)]
    pub target: PublishTarget,
    /// Branch name resolved by the controller's mirror for a source-branch push. Never taken
    /// from payload text at push time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch: Option<String>,
}

impl PublishRecord {
    /// Whether the job has been pushed to the forge, directly or via the snapshot fallback.
    pub fn is_published(&self) -> bool {
        matches!(
            self.detail.as_str(),
            "published" | "published_snapshot_fallback"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_without_a_credential_field() {
        let record = PublishRecord {
            detail: "published".to_owned(),
            branch: "snapshot/abc".to_owned(),
            remote: "forge".to_owned(),
            base_commit: "a".repeat(40),
            commit: Some("b".repeat(40)),
            gates: vec![PublishGateRecord {
                name: "secrets".to_owned(),
                job_id: "gate".to_owned(),
                result: "pass".to_owned(),
                exit_code: Some(0),
            }],
            target: PublishTarget::Snapshot,
            source_branch: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("token"));
        assert_eq!(
            serde_json::from_str::<PublishRecord>(&json).unwrap(),
            record
        );
    }
}
