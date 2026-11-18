//! Events in both directions. Controller to agent: assign, follow up, cancel. Agent to
//! controller: a log chunk and a completion. Every size is capped.

use serde::{Deserialize, Serialize};

use crate::failure::FailureClass;
use crate::ids::JobId;
use crate::limits::{MAX_LOG_CHUNK_BYTES, MAX_SUMMARY_BYTES};
use crate::spec::JobSpec;

/// A message from the controller to an agent, tagged by `kind`.
// The spec is kept inline in the wire event; boxing it would ripple through every agent.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    Assign {
        job_id: JobId,
        epoch: u64,
        spec: JobSpec,
    },
    Followup {
        job_id: JobId,
        epoch: u64,
        message: String,
    },
    Cancel {
        job_id: JobId,
        epoch: u64,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawAgentEvent {
    Assign {
        job_id: JobId,
        epoch: u64,
        spec: JobSpec,
    },
    Followup {
        job_id: JobId,
        epoch: u64,
        message: String,
    },
    Cancel {
        job_id: JobId,
        epoch: u64,
    },
}

impl TryFrom<RawAgentEvent> for AgentEvent {
    type Error = String;
    fn try_from(raw: RawAgentEvent) -> Result<Self, Self::Error> {
        Ok(match raw {
            RawAgentEvent::Assign {
                job_id,
                epoch,
                spec,
            } => AgentEvent::Assign {
                job_id,
                epoch,
                spec,
            },
            RawAgentEvent::Followup {
                job_id,
                epoch,
                message,
            } => AgentEvent::Followup {
                job_id,
                epoch,
                message,
            },
            RawAgentEvent::Cancel { job_id, epoch } => AgentEvent::Cancel { job_id, epoch },
        })
    }
}

impl<'de> Deserialize<'de> for AgentEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawAgentEvent::deserialize(deserializer)?;
        AgentEvent::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// A piece of a harness's output, streamed to the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogChunk {
    pub job_id: JobId,
    pub epoch: u64,
    pub seq: u64,
    pub data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogChunk {
    pub job_id: JobId,
    pub epoch: u64,
    pub seq: u64,
    pub data: String,
}

impl TryFrom<RawLogChunk> for LogChunk {
    type Error = LogChunkError;
    fn try_from(raw: RawLogChunk) -> Result<Self, Self::Error> {
        let lc = LogChunk {
            job_id: raw.job_id,
            epoch: raw.epoch,
            seq: raw.seq,
            data: raw.data,
        };
        lc.validate()?;
        Ok(lc)
    }
}

impl<'de> Deserialize<'de> for LogChunk {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawLogChunk::deserialize(deserializer)?;
        LogChunk::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl LogChunk {
    /// Refuses a body over [`MAX_LOG_CHUNK_BYTES`].
    pub fn validate(&self) -> Result<(), LogChunkError> {
        if self.data.len() > MAX_LOG_CHUNK_BYTES {
            return Err(LogChunkError::TooLarge {
                got: self.data.len(),
            });
        }
        Ok(())
    }
}

/// Why a log chunk was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogChunkError {
    TooLarge { got: usize },
}

impl std::fmt::Display for LogChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { got } => write!(
                f,
                "a log chunk may be at most {MAX_LOG_CHUNK_BYTES} bytes (got {got})"
            ),
        }
    }
}

impl std::error::Error for LogChunkError {}

/// How a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Failed(FailureClass),
    /// The agent stopped the job because the controller asked it to.
    Cancelled,
}

/// The agent's final report for a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Completion {
    pub job_id: JobId,
    pub epoch: u64,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// For a provider failure: when the provider is expected back, in seconds. The controller
    /// cordons the provider on the node for this long, but clears that cordon after two healthy
    /// provider heartbeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompletion {
    pub job_id: JobId,
    pub epoch: u64,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

impl TryFrom<RawCompletion> for Completion {
    type Error = CompletionError;
    fn try_from(raw: RawCompletion) -> Result<Self, Self::Error> {
        let c = Completion {
            job_id: raw.job_id,
            epoch: raw.epoch,
            outcome: raw.outcome,
            exit_code: raw.exit_code,
            summary: raw.summary,
            retry_after_secs: raw.retry_after_secs,
        };
        c.validate()?;
        Ok(c)
    }
}

impl<'de> Deserialize<'de> for Completion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawCompletion::deserialize(deserializer)?;
        Completion::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl Completion {
    /// Refuses a summary over [`MAX_SUMMARY_BYTES`].
    pub fn validate(&self) -> Result<(), CompletionError> {
        if let Some(summary) = &self.summary {
            if summary.len() > MAX_SUMMARY_BYTES {
                return Err(CompletionError::SummaryTooLarge { got: summary.len() });
            }
        }
        Ok(())
    }
}

/// Why a completion was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionError {
    SummaryTooLarge { got: usize },
}

impl std::fmt::Display for CompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SummaryTooLarge { got } => write!(
                f,
                "a completion summary may be at most {MAX_SUMMARY_BYTES} bytes (got {got})"
            ),
        }
    }
}

impl std::error::Error for CompletionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn job_id() -> JobId {
        JobId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
    }

    fn spec() -> JobSpec {
        JobSpec {
            repo: "https://example.net/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            base_commit: None,
            task: "run the tests".to_owned(),
            harness: "opencode".to_owned(),
            provider: None,
            model: None,
            timeout_secs: 3600,
            idle_timeout_secs: None,
            idempotency_key: None,
            after: Vec::new(),
            stack_dependencies: true,
        }
    }

    #[test]
    fn every_agent_event_round_trips() {
        let events = [
            AgentEvent::Assign {
                job_id: job_id(),
                epoch: 3,
                spec: spec(),
            },
            AgentEvent::Followup {
                job_id: job_id(),
                epoch: 4,
                message: "try again".to_owned(),
            },
            AgentEvent::Cancel {
                job_id: job_id(),
                epoch: 5,
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn agent_events_carry_a_kind_tag() {
        let event = AgentEvent::Cancel {
            job_id: job_id(),
            epoch: 1,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            format!(r#"{{"kind":"cancel","job_id":"{}","epoch":1}}"#, job_id())
        );
    }

    #[test]
    fn an_unknown_event_kind_is_refused() {
        let json = format!(r#"{{"kind":"pause","job_id":"{}","epoch":1}}"#, job_id());
        assert!(serde_json::from_str::<AgentEvent>(&json).is_err());
    }

    #[test]
    fn unknown_fields_in_an_event_are_refused() {
        let json = format!(
            r#"{{"kind":"cancel","job_id":"{}","epoch":1,"why":"x"}}"#,
            job_id()
        );
        assert!(serde_json::from_str::<AgentEvent>(&json).is_err());
    }

    #[test]
    fn a_log_chunk_round_trips_and_is_capped() {
        let chunk = LogChunk {
            job_id: job_id(),
            epoch: 2,
            seq: 7,
            data: "hello\n".to_owned(),
        };
        assert!(chunk.validate().is_ok());
        let json = serde_json::to_string(&chunk).unwrap();
        assert_eq!(serde_json::from_str::<LogChunk>(&json).unwrap(), chunk);

        let big = LogChunk {
            data: "x".repeat(MAX_LOG_CHUNK_BYTES + 1),
            ..chunk
        };
        assert_eq!(
            big.validate().unwrap_err(),
            LogChunkError::TooLarge {
                got: MAX_LOG_CHUNK_BYTES + 1
            }
        );
    }

    #[test]
    fn completions_round_trip_for_both_outcomes() {
        let done = Completion {
            job_id: job_id(),
            epoch: 2,
            outcome: Outcome::Done,
            exit_code: Some(0),
            summary: Some("all green".to_owned()),
            retry_after_secs: None,
        };
        assert!(done.validate().is_ok());
        let json = serde_json::to_string(&done).unwrap();
        assert_eq!(serde_json::from_str::<Completion>(&json).unwrap(), done);

        let failed = Completion {
            job_id: job_id(),
            epoch: 3,
            outcome: Outcome::Failed(FailureClass::Execution),
            exit_code: Some(2),
            summary: None,
            retry_after_secs: Some(150),
        };
        assert!(failed.validate().is_ok());
        let json = serde_json::to_string(&failed).unwrap();
        assert_eq!(serde_json::from_str::<Completion>(&json).unwrap(), failed);
    }

    #[test]
    fn an_oversized_summary_is_refused() {
        let completion = Completion {
            job_id: job_id(),
            epoch: 1,
            outcome: Outcome::Done,
            exit_code: None,
            summary: Some("s".repeat(MAX_SUMMARY_BYTES + 1)),
            retry_after_secs: None,
        };
        assert_eq!(
            completion.validate().unwrap_err(),
            CompletionError::SummaryTooLarge {
                got: MAX_SUMMARY_BYTES + 1
            }
        );
    }

    #[test]
    fn unknown_completion_fields_are_refused() {
        let json = format!(
            r#"{{"job_id":"{}","epoch":1,"outcome":"done","extra":true}}"#,
            job_id()
        );
        assert!(serde_json::from_str::<Completion>(&json).is_err());
    }

    #[test]
    fn outcome_tags_are_stable() {
        assert_eq!(serde_json::to_string(&Outcome::Done).unwrap(), "\"done\"");
        assert_eq!(
            serde_json::to_string(&Outcome::Failed(FailureClass::Provider)).unwrap(),
            r#"{"failed":"provider"}"#
        );
    }
}
