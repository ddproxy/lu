//! Job lifecycle. The allowed edges are an explicit table so a controller can never move a job
//! into a state that has no way out, and a terminal job is frozen.

use serde::{Deserialize, Serialize};

/// Where a job is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
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
}

impl JobState {
    /// Every state, in declaration order.
    pub const ALL: [JobState; 8] = [
        Self::Queued,
        Self::Leased,
        Self::Running,
        Self::Recovering,
        Self::NeedsInput,
        Self::Done,
        Self::Failed,
        Self::Cancelled,
    ];

    /// A terminal state has no outgoing edges.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    /// Whether the controller may move a job from `from` to `to`. The table test below lists
    /// every allowed edge, and terminal states have none.
    pub fn can_transition(from: Self, to: Self) -> bool {
        use JobState::{Cancelled, Done, Failed, Leased, NeedsInput, Queued, Recovering, Running};
        matches!(
            (from, to),
            (Queued, Leased)
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
                | (Recovering, Queued)
                | (Recovering, Failed)
                | (Recovering, Cancelled)
                | (NeedsInput, Running)
                | (NeedsInput, Cancelled)
                | (NeedsInput, Failed)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::JobState::*;
    use super::*;

    #[test]
    fn the_transition_table_is_exactly_the_allowed_edges() {
        let allowed = [
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
                matches!(state, Done | Failed | Cancelled),
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
}
