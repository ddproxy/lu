//! Why a job stopped. A node failure and a provider failure are both worth one more try; an
//! execution failure is the job's own outcome and is never retried.

use serde::{Deserialize, Serialize};

/// The class of a failure, which decides whether the job is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The agent was lost, the lease expired, or the node could not stage or start the job.
    Node,
    /// Every key was unavailable, a model limit was hit, or a 429.
    Provider,
    /// The harness exited non-zero, timed out, or produced no output.
    Execution,
    /// A dependency finished unsuccessfully; a dependent cannot retry into a different outcome.
    Dependency,
}

/// A provider outage is not the job's fault, so it does not use the retry budget: the job goes
/// back to the queue and waits, and the provider cordon keeps it off nodes that cannot serve it.
/// This only bounds a job that fails this way over and over for some other reason.
pub const PROVIDER_ATTEMPT_LIMIT: u32 = 100;

impl FailureClass {
    /// Node failures retry while the attempt count is within the cap, provider failures until
    /// [`PROVIDER_ATTEMPT_LIMIT`]; an execution failure never does.
    pub fn retryable(self, attempt: u32, max_retries: u32) -> bool {
        match self {
            Self::Node => attempt <= max_retries,
            Self::Provider => attempt <= PROVIDER_ATTEMPT_LIMIT,
            Self::Execution | Self::Dependency => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_failures_retry_within_the_cap() {
        let class = FailureClass::Node;
        assert!(class.retryable(1, 1));
        assert!(!class.retryable(2, 1));
        assert!(class.retryable(1, 2));
        assert!(class.retryable(2, 2));
        assert!(!class.retryable(3, 2));
    }

    #[test]
    fn a_provider_outage_does_not_use_the_retry_budget() {
        let class = FailureClass::Provider;
        assert!(class.retryable(1, 0));
        assert!(class.retryable(5, 1));
        assert!(class.retryable(PROVIDER_ATTEMPT_LIMIT, 1));
        assert!(!class.retryable(PROVIDER_ATTEMPT_LIMIT + 1, 1));
    }

    #[test]
    fn an_execution_failure_never_retries() {
        for attempt in 1..=5 {
            assert!(!FailureClass::Execution.retryable(attempt, 5));
        }
        assert!(!FailureClass::Execution.retryable(1, 0));
        assert!(!FailureClass::Dependency.retryable(5, 5));
    }

    #[test]
    fn classes_round_trip_as_snake_case_strings() {
        for class in [
            FailureClass::Node,
            FailureClass::Provider,
            FailureClass::Execution,
            FailureClass::Dependency,
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
