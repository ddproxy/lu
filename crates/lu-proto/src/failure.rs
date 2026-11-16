//! Why a job stopped. A node failure and a provider failure are both worth one more try; an
//! execution failure is the job's own outcome and is never retried.

use serde::{Deserialize, Serialize};

/// The class of a failure, which decides whether the job is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The agent was lost or the lease expired.
    Node,
    /// Every key was unavailable, a model limit was hit, or a 429.
    Provider,
    /// The harness exited non-zero, timed out, or produced no output.
    Execution,
}

impl FailureClass {
    /// Node and provider failures retry while the attempt count is within the cap; an execution
    /// failure never does.
    pub fn retryable(self, attempt: u32, max_retries: u32) -> bool {
        match self {
            Self::Node | Self::Provider => attempt <= max_retries,
            Self::Execution => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_and_provider_failures_retry_within_the_cap() {
        for class in [FailureClass::Node, FailureClass::Provider] {
            assert!(class.retryable(1, 1));
            assert!(!class.retryable(2, 1));
            assert!(class.retryable(1, 2));
            assert!(class.retryable(2, 2));
            assert!(!class.retryable(3, 2));
        }
    }

    #[test]
    fn an_execution_failure_never_retries() {
        for attempt in 1..=5 {
            assert!(!FailureClass::Execution.retryable(attempt, 5));
        }
        assert!(!FailureClass::Execution.retryable(1, 0));
    }

    #[test]
    fn classes_round_trip_as_snake_case_strings() {
        for class in [
            FailureClass::Node,
            FailureClass::Provider,
            FailureClass::Execution,
        ] {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(serde_json::from_str::<FailureClass>(&json).unwrap(), class);
        }
        assert_eq!(
            serde_json::to_string(&FailureClass::Execution).unwrap(),
            "\"execution\""
        );
    }
}
