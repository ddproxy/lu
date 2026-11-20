//! Size caps for every field that crosses the wire, and the fixed artifact names the controller
//! chooses. The controller picks artifact paths, never the agent, so a path can only ever be one
//! of the three names below under a job id.

use crate::JobId;

/// A job id is 26 Crockford base32 characters: a 48-bit millisecond prefix, then 80 random bits.
pub const JOB_ID_LEN: usize = 26;

/// Node, harness and provider names.
pub const MAX_NAME_BYTES: usize = 64;

/// An agent's reported version string.
pub const MAX_VERSION_BYTES: usize = 32;

pub const MAX_REPO_BYTES: usize = 2048;
pub const MAX_GIT_REF_BYTES: usize = 256;
pub const MAX_TASK_BYTES: usize = 256 * 1024;
pub const MAX_MODEL_BYTES: usize = 128;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// The most dependencies one job may name.
pub const MAX_AFTER_JOBS: usize = 16;

/// The most dependency artifact directories one job may collect into `.lu/inputs`.
pub const MAX_COLLECT_INPUTS: usize = 16;

/// The largest combined size of collected diffs accepted at submission and re-checked at stage
/// time. One job's own diff is still capped by [`MAX_DIFF_BYTES`].
pub const MAX_COLLECT_BYTES: u64 = 64 * 1024 * 1024;

/// Collected logs keep only their tail, so one chatty dependency cannot fill the tree.
pub const INPUT_LOG_BYTES: usize = 256 * 1024;

/// The most alternative provider choices one job may name.
pub const MAX_FALLBACKS: usize = 4;

pub const MIN_TIMEOUT_SECS: u64 = 1;
pub const MAX_TIMEOUT_SECS: u64 = 86_400;
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86_400;
pub const DEFAULT_TIMEOUT_SECS: u64 = 3_600;

/// At most this many providers may appear in one heartbeat.
pub const MAX_PROVIDERS: usize = 64;

/// One node label or job requirement, in bytes.
pub const MAX_LABEL_BYTES: usize = 64;

/// The most labels one node may advertise.
pub const MAX_NODE_LABELS: usize = 32;

/// The most labels one job may require.
pub const MAX_REQUIRES_LABELS: usize = 8;

/// The most taints one node may advertise.
pub const MAX_TAINTS: usize = 8;

/// The most taints one job may tolerate.
pub const MAX_TOLERATES: usize = 8;

/// The most label files one agent may watch.
pub const MAX_LABEL_FILES: usize = 8;

/// One label file's largest accepted size.
pub const MAX_LABEL_FILE_BYTES: u64 = 4 * 1024;

/// The forwarded Reiki health document, serialized.
pub const MAX_REIKI_BYTES: usize = 64 * 1024;

/// Host and job stats in one heartbeat, serialized. Processes are dropped before jobs when
/// the combined document would exceed this.
pub const MAX_STATS_BYTES: usize = 16 * 1024;

/// One log chunk body.
pub const MAX_LOG_CHUNK_BYTES: usize = 64 * 1024;

/// A completion summary.
pub const MAX_SUMMARY_BYTES: usize = 4 * 1024;

/// Per-file artifact caps, as enforced by the controller.
pub const MAX_DIFF_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_REPORT_BYTES: u64 = 1024 * 1024;

/// Why a string was not one of the three artifact file names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseArtifactNameError;

impl std::fmt::Display for ParseArtifactNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a recognised artifact name")
    }
}

impl std::error::Error for ParseArtifactNameError {}

/// The three artifact files a job may upload. Parsing accepts only these fixed names, so an agent
/// can never choose an arbitrary path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactName {
    DiffPatch,
    RunLog,
    ReportJson,
}

impl ArtifactName {
    /// Every artifact, in upload order.
    pub const ALL: [ArtifactName; 3] = [Self::DiffPatch, Self::RunLog, Self::ReportJson];

    /// The fixed file name, with no directory part.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::DiffPatch => "diff.patch",
            Self::RunLog => "run.log",
            Self::ReportJson => "report.json",
        }
    }

    /// The most bytes this artifact may hold.
    pub fn max_bytes(self) -> u64 {
        match self {
            Self::DiffPatch => MAX_DIFF_BYTES,
            Self::RunLog => MAX_LOG_BYTES,
            Self::ReportJson => MAX_REPORT_BYTES,
        }
    }

    /// The controller-chosen path for this artifact of `job_id`: `<job id>/<file name>`.
    pub fn relative_path(self, job_id: &JobId) -> String {
        format!("{job_id}/{}", self.file_name())
    }
}

impl std::fmt::Display for ArtifactName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.file_name())
    }
}

impl std::str::FromStr for ArtifactName {
    type Err = ParseArtifactNameError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(match text {
            "diff.patch" => Self::DiffPatch,
            "run.log" => Self::RunLog,
            "report.json" => Self::ReportJson,
            _ => return Err(ParseArtifactNameError),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn artifact_names_are_the_three_fixed_files() {
        assert_eq!(ArtifactName::DiffPatch.file_name(), "diff.patch");
        assert_eq!(ArtifactName::RunLog.file_name(), "run.log");
        assert_eq!(ArtifactName::ReportJson.file_name(), "report.json");
        assert_eq!(ArtifactName::ALL.len(), 3);
        for name in ArtifactName::ALL {
            let file = name.file_name();
            assert!(!file.contains('/') && !file.contains('\\'), "{file}");
            assert!(!file.contains(".."), "{file}");
        }
    }

    #[test]
    fn artifact_names_round_trip_through_display_and_from_str() {
        for name in ArtifactName::ALL {
            let text = name.to_string();
            assert_eq!(ArtifactName::from_str(&text).unwrap(), name, "{text}");
            assert_eq!(text, name.file_name());
        }
        assert!("../escape".parse::<ArtifactName>().is_err());
    }

    #[test]
    fn artifact_paths_are_built_only_from_a_job_id_and_a_fixed_name() {
        let id = JobId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(
            ArtifactName::DiffPatch.relative_path(&id),
            "01ARZ3NDEKTSV4RRFFQ69G5FAV/diff.patch"
        );
        for name in ArtifactName::ALL {
            let path = name.relative_path(&id);
            assert_eq!(path.matches('/').count(), 1, "{path}");
            assert!(path.starts_with(&id.to_string()));
        }
    }

    #[test]
    fn artifact_caps_are_the_documented_ones() {
        assert_eq!(ArtifactName::DiffPatch.max_bytes(), 10 * 1024 * 1024);
        assert_eq!(ArtifactName::RunLog.max_bytes(), 50 * 1024 * 1024);
        assert_eq!(ArtifactName::ReportJson.max_bytes(), 1024 * 1024);
    }
}
