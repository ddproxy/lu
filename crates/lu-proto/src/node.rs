//! Node heartbeat: what a peer says about itself. The forwarded Reiki document is opaque and
//! capped, and provider names are checked with the same rule as node names.

use serde::{Deserialize, Serialize};

use crate::ids::{is_name_byte, JobId, NodeName};
use crate::label::valid_label;
use crate::limits::{
    MAX_LABEL_BYTES, MAX_NAME_BYTES, MAX_NODE_LABELS, MAX_PROVIDERS, MAX_REIKI_BYTES,
    MAX_STATS_BYTES, MAX_VERSION_BYTES,
};

/// Whether the peer's harness is free to take a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessState {
    Idle,
    Busy,
}

/// A provider's availability on this peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Ok,
    Cooling,
    Disabled,
    Unknown,
}

/// One provider and its state, as reported by the peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderStatus {
    pub name: String,
    pub state: ProviderState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soonest_secs: Option<u64>,
}

/// The host numbers sampled from `/proc`. The disk fields are optional because a portable
/// answer is not available without a platform filesystem call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostStats {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub swap_used_kb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    pub uptime_secs: u64,
}

/// One process in a job's harness tree, as sampled from `/proc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessStats {
    pub pid: u32,
    pub ppid: u32,
    pub state: String,
    pub cpu_secs: u64,
    pub rss_kb: u64,
    pub age_secs: u64,
    pub cmdline: String,
    pub open_files: u32,
}

/// One running job and the processes in its harness process group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStats {
    pub job_id: JobId,
    pub pgid: u32,
    pub output_idle_secs: u64,
    pub cpu_idle_secs: u64,
    pub stuck: bool,
    pub processes: Vec<ProcessStats>,
}

/// The stats a node reports: its host numbers and its running jobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
}

/// A periodic message from an agent. `reiki` is the opaque health document, forwarded unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heartbeat {
    pub node: NodeName,
    pub harness: HarnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reiki: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeartbeat {
    pub node: NodeName,
    pub harness: HarnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reiki: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

impl TryFrom<RawHeartbeat> for Heartbeat {
    type Error = HeartbeatError;
    fn try_from(raw: RawHeartbeat) -> Result<Self, Self::Error> {
        let mut hb = Heartbeat {
            node: raw.node,
            harness: raw.harness,
            version: raw.version,
            providers: raw.providers,
            reiki: raw.reiki,
            host: raw.host,
            jobs: raw.jobs,
            labels: raw.labels,
        };
        hb.trim_stats();
        hb.validate()?;
        Ok(hb)
    }
}

impl<'de> Deserialize<'de> for Heartbeat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawHeartbeat::deserialize(deserializer)?;
        Heartbeat::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl Heartbeat {
    /// Drops process entries first, then whole jobs, until the serialized stats fit the cap.
    /// The order removes the least busy processes first within each job, because a sampler
    /// lists processes busiest-first and this pops from the end of the list.
    pub fn trim_stats(&mut self) {
        while self.stats_len() > MAX_STATS_BYTES {
            let mut dropped_process = false;
            for job in self.jobs.iter_mut().rev() {
                if job.processes.pop().is_some() {
                    dropped_process = true;
                    break;
                }
            }
            if !dropped_process && self.jobs.pop().is_none() {
                break;
            }
        }
    }

    fn stats_len(&self) -> usize {
        if self.host.is_none() && self.jobs.is_empty() {
            return 0;
        }
        let stats = NodeStats {
            host: self.host.clone(),
            jobs: self.jobs.clone(),
        };
        serde_json::to_vec(&stats)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    }

    /// Checks the caps a single heartbeat must respect.
    pub fn validate(&self) -> Result<(), HeartbeatError> {
        if let Some(version) = &self.version {
            if version.is_empty()
                || version.len() > MAX_VERSION_BYTES
                || !version
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ')
            {
                return Err(HeartbeatError::Version {
                    value: version.clone(),
                });
            }
        }
        if self.providers.len() > MAX_PROVIDERS {
            return Err(HeartbeatError::TooManyProviders {
                got: self.providers.len(),
            });
        }
        for (index, provider) in self.providers.iter().enumerate() {
            if !valid_provider_name(&provider.name) {
                return Err(HeartbeatError::ProviderName {
                    index,
                    name: provider.name.clone(),
                });
            }
        }
        if self.labels.len() > MAX_NODE_LABELS {
            return Err(HeartbeatError::TooManyLabels {
                got: self.labels.len(),
            });
        }
        for label in &self.labels {
            if !valid_label(label) {
                return Err(HeartbeatError::Label {
                    value: label.clone(),
                });
            }
        }
        if let Some(reiki) = &self.reiki {
            let size = serde_json::to_vec(reiki)
                .map_err(|_| HeartbeatError::ReikiNotSerializable)?
                .len();
            if size > MAX_REIKI_BYTES {
                return Err(HeartbeatError::ReikiTooLarge { got: size });
            }
        }
        Ok(())
    }
}

/// Why a heartbeat was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatError {
    Version { value: String },
    TooManyProviders { got: usize },
    ProviderName { index: usize, name: String },
    TooManyLabels { got: usize },
    Label { value: String },
    ReikiTooLarge { got: usize },
    ReikiNotSerializable,
}

impl std::fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version { value } => write!(
                f,
                "the agent version {value:?} must be 1 to {MAX_VERSION_BYTES} printable ASCII characters"
            ),
            Self::TooManyProviders { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_PROVIDERS} providers (got {got})"
            ),
            Self::ProviderName { index, name } => write!(
                f,
                "provider #{index} has an invalid name {name:?}; it must be 1 to {MAX_NAME_BYTES} characters from A-Z a-z 0-9 . _ -"
            ),
            Self::TooManyLabels { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_NODE_LABELS} labels (got {got})"
            ),
            Self::Label { value } => write!(
                f,
                "invalid node label {value:?}; it must be 1 to {MAX_LABEL_BYTES} characters from A-Z a-z 0-9 . _ - / with at most one '='"
            ),
            Self::ReikiTooLarge { got } => write!(
                f,
                "the forwarded reiki document is larger than {MAX_REIKI_BYTES} bytes (got {got})"
            ),
            Self::ReikiNotSerializable => {
                f.write_str("the forwarded reiki document could not be serialised")
            }
        }
    }
}

impl std::error::Error for HeartbeatError {}

/// Whether `name` may appear as a provider in a heartbeat.
pub fn valid_provider_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && name.bytes().all(is_name_byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn heartbeat() -> Heartbeat {
        Heartbeat {
            node: NodeName::from_str("peer-a").unwrap(),
            harness: HarnessState::Idle,
            version: None,
            providers: vec![ProviderStatus {
                name: "example-provider".to_owned(),
                state: ProviderState::Ok,
                soonest_secs: None,
            }],
            reiki: None,
            host: None,
            jobs: Vec::new(),
            labels: Vec::new(),
        }
    }

    fn host() -> HostStats {
        HostStats {
            load1: 1.5,
            load5: 1.25,
            load15: 1.0,
            mem_total_kb: 16_384,
            mem_available_kb: 8_192,
            swap_used_kb: 0,
            disk_free_bytes: Some(1_000_000),
            disk_total_bytes: Some(8_000_000),
            uptime_secs: 42,
        }
    }

    fn job(job_id: &str) -> JobStats {
        JobStats {
            job_id: JobId::from_str(job_id).unwrap(),
            pgid: 123,
            output_idle_secs: 301,
            cpu_idle_secs: 302,
            stuck: true,
            processes: vec![ProcessStats {
                pid: 123,
                ppid: 1,
                state: "R".to_owned(),
                cpu_secs: 7,
                rss_kb: 2048,
                age_secs: 30,
                cmdline: "opencode run".to_owned(),
                open_files: 12,
            }],
        }
    }

    #[test]
    fn a_heartbeat_round_trips() {
        let mut beat = heartbeat();
        beat.harness = HarnessState::Busy;
        beat.providers[0].state = ProviderState::Cooling;
        beat.providers[0].soonest_secs = Some(30);
        beat.reiki = Some(serde_json::json!({"status": "ok"}));
        assert!(beat.validate().is_ok());
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, beat);
    }

    #[test]
    fn providers_default_to_empty_and_are_omitted() {
        let json = r#"{"node":"peer-a","harness":"idle"}"#;
        let beat: Heartbeat = serde_json::from_str(json).unwrap();
        assert!(beat.providers.is_empty());
        assert!(beat.reiki.is_none());
        assert!(beat.validate().is_ok());
        let out = serde_json::to_string(&beat).unwrap();
        assert!(!out.contains("providers"), "{out}");
        assert!(!out.contains("reiki"), "{out}");
    }

    #[test]
    fn an_agent_version_is_optional_and_checked() {
        let mut beat = heartbeat();
        beat.version = Some("0.1.10".to_owned());
        assert!(beat.validate().is_ok());
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version.as_deref(), Some("0.1.10"));

        for version in [
            String::new(),
            "has\nnewline".to_owned(),
            "café".to_owned(),
            "a".repeat(MAX_VERSION_BYTES + 1),
        ] {
            beat.version = Some(version);
            assert!(matches!(
                beat.validate().unwrap_err(),
                HeartbeatError::Version { .. }
            ));
        }
        beat.version = Some("a".repeat(MAX_VERSION_BYTES));
        assert!(beat.validate().is_ok());
    }

    #[test]
    fn too_many_providers_are_refused() {
        let mut beat = heartbeat();
        beat.providers = (0..=MAX_PROVIDERS)
            .map(|index| ProviderStatus {
                name: format!("provider-{index}"),
                state: ProviderState::Unknown,
                soonest_secs: None,
            })
            .collect();
        assert_eq!(
            beat.validate().unwrap_err(),
            HeartbeatError::TooManyProviders {
                got: MAX_PROVIDERS + 1
            }
        );
    }

    #[test]
    fn a_bad_provider_name_is_refused() {
        for name in ["", "bad name", "peer/../a", "café", "peer\u{1b}[31m"] {
            let mut beat = heartbeat();
            beat.providers[0].name = name.to_owned();
            assert!(matches!(
                beat.validate().unwrap_err(),
                HeartbeatError::ProviderName { .. }
            ));
        }
        let mut beat = heartbeat();
        beat.providers[0].name = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::ProviderName { .. }
        ));
    }

    #[test]
    fn node_labels_round_trip_and_are_omitted_when_empty() {
        let mut beat = heartbeat();
        assert!(!serde_json::to_string(&beat).unwrap().contains("labels"));
        beat.labels = vec!["php8.3".to_owned(), "region=eu".to_owned()];
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.labels, beat.labels);
        assert!(beat.validate().is_ok());
    }

    #[test]
    fn too_many_or_invalid_node_labels_are_refused() {
        let mut beat = heartbeat();
        beat.labels = (0..=MAX_NODE_LABELS)
            .map(|i| format!("label-{i}"))
            .collect();
        assert_eq!(
            beat.validate().unwrap_err(),
            HeartbeatError::TooManyLabels {
                got: MAX_NODE_LABELS + 1
            }
        );
        for label in [
            "",
            "bad label",
            "café",
            "a=b=c",
            &"a".repeat(MAX_LABEL_BYTES + 1),
        ] {
            beat.labels = vec![label.to_owned()];
            assert!(
                matches!(beat.validate().unwrap_err(), HeartbeatError::Label { .. }),
                "{label:?}"
            );
        }
    }

    #[test]
    fn old_heartbeats_without_labels_still_parse() {
        let beat: Heartbeat =
            serde_json::from_str(r#"{"node":"peer-a","harness":"idle","providers":[]}"#).unwrap();
        assert!(beat.labels.is_empty());
    }

    #[test]
    fn an_oversized_reiki_document_is_refused() {
        let mut beat = heartbeat();
        beat.reiki = Some(serde_json::Value::String("a".repeat(MAX_REIKI_BYTES)));
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::ReikiTooLarge { .. }
        ));
    }

    #[test]
    fn host_and_job_stats_round_trip() {
        let mut beat = heartbeat();
        beat.host = Some(host());
        beat.jobs = vec![job("01ARZ3NDEKTSV4RRFFQ69G5FAV")];
        assert!(beat.validate().is_ok());
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, beat);
    }

    #[test]
    fn disk_stats_round_trip_and_stay_optional() {
        let mut beat = heartbeat();
        let mut host = host();
        beat.host = Some(host.clone());
        let json = serde_json::to_string(&beat).unwrap();
        assert!(json.contains("\"disk_free_bytes\":1000000"), "{json}");
        assert!(json.contains("\"disk_total_bytes\":8000000"), "{json}");
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.host, Some(host.clone()));

        host.disk_free_bytes = None;
        host.disk_total_bytes = None;
        beat.host = Some(host);
        let json = serde_json::to_string(&beat).unwrap();
        assert!(!json.contains("disk_"), "{json}");
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert!(parsed.host.is_some());
        assert!(parsed.host.unwrap().disk_free_bytes.is_none());
    }

    #[test]
    fn heartbeats_without_stats_keep_the_old_shape() {
        let beat = heartbeat();
        let out = serde_json::to_string(&beat).unwrap();
        assert!(!out.contains("\"host\""), "{out}");
        assert!(!out.contains("\"jobs\""), "{out}");
        let parsed: Heartbeat =
            serde_json::from_str(r#"{"node":"peer-a","harness":"idle"}"#).unwrap();
        assert!(parsed.host.is_none());
        assert!(parsed.jobs.is_empty());
    }

    #[test]
    fn oversized_stats_drop_processes_then_jobs() {
        let mut beat = heartbeat();
        beat.host = Some(host());
        let mut big = job("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        big.processes = (0..300)
            .map(|index| ProcessStats {
                pid: index,
                ppid: 1,
                state: "S".to_owned(),
                cpu_secs: u64::from(index),
                rss_kb: 1,
                age_secs: 1,
                cmdline: "x".repeat(80),
                open_files: 0,
            })
            .collect();
        beat.jobs = vec![big];
        beat.trim_stats();
        assert!(!beat.jobs.is_empty());
        assert!(beat.jobs[0].processes.len() < 300);
        assert!(beat.stats_len() <= MAX_STATS_BYTES);

        // Even with no process entries left, whole jobs are dropped.
        beat.jobs = (0..200)
            .map(|_| {
                let mut job = job("01ARZ3NDEKTSV4RRFFQ69G5FAV");
                job.processes.clear();
                job
            })
            .collect();
        beat.trim_stats();
        assert!(beat.stats_len() <= MAX_STATS_BYTES);
        assert!(beat.jobs.len() < 200);
    }

    #[test]
    fn an_incoming_oversized_heartbeat_is_trimmed() {
        let mut beat = heartbeat();
        beat.host = Some(host());
        beat.jobs = vec![job("01ARZ3NDEKTSV4RRFFQ69G5FAV")];
        beat.jobs[0].processes = (0..300)
            .map(|index| ProcessStats {
                pid: index,
                ppid: 1,
                state: "S".to_owned(),
                cpu_secs: u64::from(index),
                rss_kb: 1,
                age_secs: 1,
                cmdline: "y".repeat(80),
                open_files: 0,
            })
            .collect();
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert!(parsed.jobs[0].processes.len() < 300);
        let stats = NodeStats {
            host: parsed.host,
            jobs: parsed.jobs,
        };
        assert!(serde_json::to_vec(&stats).unwrap().len() <= MAX_STATS_BYTES);
    }

    #[test]
    fn unknown_heartbeat_fields_are_refused() {
        let json = r#"{"node":"peer-a","harness":"idle","surprise":true}"#;
        assert!(serde_json::from_str::<Heartbeat>(json).is_err());
    }

    #[test]
    fn provider_states_round_trip_as_snake_case_strings() {
        for state in [
            ProviderState::Ok,
            ProviderState::Cooling,
            ProviderState::Disabled,
            ProviderState::Unknown,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<ProviderState>(&json).unwrap(), state);
        }
        for state in [HarnessState::Idle, HarnessState::Busy] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<HarnessState>(&json).unwrap(), state);
        }
    }
}
