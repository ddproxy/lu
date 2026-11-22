//! Job lifecycle. The allowed edges are an explicit table so a controller can never move a job
//! into a state that has no way out, and a terminal job is frozen.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a string was not a recognised job state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseJobStateError;

impl fmt::Display for ParseJobStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a recognised job state")
    }
}

impl std::error::Error for ParseJobStateError {}

/// Where a job is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Stored and visible, but never dispatched until an operator approves it.
    Draft,
    /// Submitted, but at least one dependency is not finished.
    Blocked,
    Queued,
    Leased,
    Running,
    /// A lease was lost or a node failed; the controller is deciding what to do next.
    Recovering,
    /// Reserved: a harness is waiting on input from a human.
    NeedsInput,
    Done,
    Failed,
    Cancelled,
    /// A draft that was never approved before its TTL elapsed.
    Expired,
}

/// What cancelling a job in a given state does. One rule for every store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelEffect {
    /// No agent holds the job: it ends `cancelled` now.
    Now,
    /// An agent holds it: record the request and let the agent stop it.
    Request,
    /// The job already ended.
    Refuse,
}

impl JobState {
    /// The wire spelling documented in `docs/api.md`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Blocked => "blocked",
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Running => "running",
            Self::Recovering => "recovering",
            Self::NeedsInput => "needs_input",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }

    /// Parses the wire spelling documented in `docs/api.md`.
    pub fn parse(text: &str) -> Result<Self, ParseJobStateError> {
        Ok(match text {
            "draft" => Self::Draft,
            "blocked" => Self::Blocked,
            "queued" => Self::Queued,
            "leased" => Self::Leased,
            "running" => Self::Running,
            "recovering" => Self::Recovering,
            "needs_input" => Self::NeedsInput,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "expired" => Self::Expired,
            _ => return Err(ParseJobStateError),
        })
    }

    /// What a cancel does in this state.
    pub fn cancel_effect(self) -> CancelEffect {
        match self {
            Self::Draft | Self::Blocked | Self::Queued | Self::Recovering => CancelEffect::Now,
            Self::Leased | Self::Running | Self::NeedsInput => CancelEffect::Request,
            Self::Done | Self::Failed | Self::Cancelled | Self::Expired => CancelEffect::Refuse,
        }
    }

    /// Every state, in declaration order.
    pub const ALL: [JobState; 11] = [
        Self::Draft,
        Self::Blocked,
        Self::Queued,
        Self::Leased,
        Self::Running,
        Self::Recovering,
        Self::NeedsInput,
        Self::Done,
        Self::Failed,
        Self::Cancelled,
        Self::Expired,
    ];

    /// A terminal state has no outgoing edges.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Cancelled | Self::Expired
        )
    }

    /// Whether the controller may move a job from `from` to `to`. The table test below lists
    /// every allowed edge, and terminal states have none.
    // Running -> Queued is the lease-lost edge, gated by the lease epoch; NeedsInput is reserved and unreachable in this version.
    /// `Recovering -> Running` and `-> Done` are the agent that kept its lease through a
    /// controller restart, resuming or finishing.
    pub fn can_transition(from: Self, to: Self) -> bool {
        use JobState::{
            Blocked, Cancelled, Done, Draft, Expired, Failed, Leased, NeedsInput, Queued,
            Recovering, Running,
        };
        matches!(
            (from, to),
            (Draft, Blocked)
                | (Draft, Queued)
                | (Draft, Cancelled)
                | (Draft, Expired)
                | (Blocked, Queued)
                | (Blocked, Cancelled)
                | (Blocked, Failed)
                | (Queued, Leased)
                | (Queued, Cancelled)
                | (Leased, Running)
                | (Leased, Queued)
                | (Leased, Recovering)
                | (Leased, Cancelled)
                | (Leased, Failed)
                | (Running, Done)
                | (Running, Failed)
                | (Running, Cancelled)
                | (Running, Queued)
                | (Running, Recovering)
                | (Recovering, Leased)
                | (Recovering, Running)
                | (Recovering, Done)
                | (Recovering, Queued)
                | (Recovering, Failed)
                | (Recovering, Cancelled)
                | (NeedsInput, Running)
                | (NeedsInput, Cancelled)
                | (NeedsInput, Failed)
        )
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobState {
    type Err = ParseJobStateError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

#[cfg(test)]
mod tests {
    use super::JobState::*;
    use super::*;

    #[test]
    fn the_transition_table_is_exactly_the_allowed_edges() {
        let allowed = [
            (Draft, Blocked),
            (Draft, Queued),
            (Draft, Cancelled),
            (Draft, Expired),
            (Blocked, Queued),
            (Blocked, Cancelled),
            (Blocked, Failed),
            (Queued, Leased),
            (Queued, Cancelled),
            (Leased, Running),
            (Leased, Queued),
            (Leased, Recovering),
            (Leased, Cancelled),
            (Leased, Failed),
            (Running, Done),
            (Running, Failed),
            (Running, Cancelled),
            (Running, Queued),
            (Running, Recovering),
            (Recovering, Leased),
            (Recovering, Running),
            (Recovering, Done),
            (Recovering, Queued),
            (Recovering, Failed),
            (Recovering, Cancelled),
            (NeedsInput, Running),
            (NeedsInput, Cancelled),
            (NeedsInput, Failed),
        ];
        for from in JobState::ALL {
            for to in JobState::ALL {
                assert_eq!(
                    JobState::can_transition(from, to),
                    allowed.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn terminal_states_have_no_outgoing_edges() {
        for from in JobState::ALL {
            if !from.is_terminal() {
                continue;
            }
            for to in JobState::ALL {
                assert!(!JobState::can_transition(from, to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn the_terminal_states_are_the_finished_ones() {
        for state in JobState::ALL {
            assert_eq!(
                state.is_terminal(),
                matches!(state, Done | Failed | Cancelled | Expired),
                "{state:?}"
            );
        }
    }

    #[test]
    fn states_round_trip_as_snake_case_strings() {
        for state in JobState::ALL {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<JobState>(&json).unwrap(), state);
        }
        assert_eq!(
            serde_json::to_string(&NeedsInput).unwrap(),
            "\"needs_input\""
        );
        assert!(serde_json::from_str::<JobState>("\"paused\"").is_err());
    }

    #[test]
    fn every_job_state_round_trips_through_as_str_and_from_str() {
        for state in JobState::ALL {
            let text = state.as_str();
            assert_eq!(JobState::from_str(text).unwrap(), state, "{text}");
            assert_eq!(state.to_string(), text);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{text}\"")
            );
        }
        assert_eq!("needs_input".parse::<JobState>().unwrap(), NeedsInput);
        assert!("paused".parse::<JobState>().is_err());
    }

    #[test]
    fn job_state_strings_match_docs_api_md() {
        // docs/api.md, "JobView": one of `draft`, `blocked`, `queued`, `leased`, `running`,
        // `recovering`, `needs_input`, `done`, `failed`, `cancelled`, `expired`.
        let documented = [
            "draft",
            "blocked",
            "queued",
            "leased",
            "running",
            "recovering",
            "needs_input",
            "done",
            "failed",
            "cancelled",
            "expired",
        ];
        let actual: Vec<&str> = JobState::ALL.iter().map(|state| state.as_str()).collect();
        assert_eq!(actual, documented);
    }
}
