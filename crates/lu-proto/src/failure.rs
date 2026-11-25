//! Why a job stopped. A node failure and a provider failure are both worth one more try; an
//! execution failure is the job's own outcome and is never retried.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a string was not a recognised failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseFailureClassError;

impl fmt::Display for ParseFailureClassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a recognised failure class")
    }
}

impl std::error::Error for ParseFailureClassError {}

/// Why an agent called a failure a node fault. Codes are fixed so they can be stored, shown in
/// job views and written to audit records without carrying task text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeFailureReason {
    /// The harness server process could not be reached.
    HarnessUnreachable,
    /// Podman refused to start or create the harness container.
    ContainerStart,
    /// The agent timed out waiting for the container's storage file.
    ContainerWaitTimeout,
    /// The configured gateway health endpoint answered 503 with an operator marker.
    #[serde(rename = "gateway_unavailable")]
    GatewayUnavailable,
    /// Preparing the workspace ran out of disk space.
    WorkspaceEnospc,
}

impl NodeFailureReason {
    /// The wire spelling stored in completions and job views.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HarnessUnreachable => "harness_unreachable",
            Self::ContainerStart => "container_start",
            Self::ContainerWaitTimeout => "container_wait_timeout",
            Self::GatewayUnavailable => "gateway_unavailable",
            Self::WorkspaceEnospc => "workspace_enospc",
        }
    }

    /// Every reason, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::HarnessUnreachable,
        Self::ContainerStart,
        Self::ContainerWaitTimeout,
        Self::GatewayUnavailable,
        Self::WorkspaceEnospc,
    ];

    /// Parses the wire spelling.
    pub fn parse(text: &str) -> Result<Self, ParseFailureClassError> {
        Ok(match text {
            "harness_unreachable" => Self::HarnessUnreachable,
            "container_start" => Self::ContainerStart,
            "container_wait_timeout" => Self::ContainerWaitTimeout,
            "gateway_unavailable" => Self::GatewayUnavailable,
            "workspace_enospc" => Self::WorkspaceEnospc,
            _ => return Err(ParseFailureClassError),
        })
    }
}

impl fmt::Display for NodeFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NodeFailureReason {
    type Err = ParseFailureClassError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

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
    /// A dependency diff could not be applied cleanly to the dependent's base tree.
    Conflict,
}

/// A provider outage is not the job's fault, so it does not use the retry budget: the job goes
/// back to the queue and waits, and the provider cordon keeps it off nodes that cannot serve it.
/// This only bounds a job that fails this way over and over for some other reason.
pub const PROVIDER_ATTEMPT_LIMIT: u32 = 100;

impl FailureClass {
    /// The wire spelling documented in `docs/api.md`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Provider => "provider",
            Self::Execution => "execution",
            Self::Dependency => "dependency",
            Self::Conflict => "conflict",
        }
    }

    /// Every class, in declaration order.
    pub const ALL: [FailureClass; 5] = [
        Self::Node,
        Self::Provider,
        Self::Execution,
        Self::Dependency,
        Self::Conflict,
    ];

    /// Parses the wire spelling documented in `docs/api.md`.
    pub fn parse(text: &str) -> Result<Self, ParseFailureClassError> {
        Ok(match text {
            "node" => Self::Node,
            "provider" => Self::Provider,
            "execution" => Self::Execution,
            "dependency" => Self::Dependency,
            "conflict" => Self::Conflict,
            _ => return Err(ParseFailureClassError),
        })
    }

    /// Node failures retry while the attempt count is within the cap, provider failures until
    /// [`PROVIDER_ATTEMPT_LIMIT`]; an execution failure never does.
    pub fn retryable(self, attempt: u32, max_retries: u32) -> bool {
        match self {
            Self::Node => attempt <= max_retries,
            Self::Provider => attempt <= PROVIDER_ATTEMPT_LIMIT,
            Self::Execution | Self::Dependency | Self::Conflict => false,
        }
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FailureClass {
    type Err = ParseFailureClassError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
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
        assert!(!FailureClass::Conflict.retryable(1, 5));
    }

    #[test]
    fn classes_from_older_agents_still_deserialize() {
        for (json, class) in [
            ("\"node\"", FailureClass::Node),
            ("\"provider\"", FailureClass::Provider),
            ("\"execution\"", FailureClass::Execution),
            ("\"dependency\"", FailureClass::Dependency),
        ] {
            assert_eq!(serde_json::from_str::<FailureClass>(json).unwrap(), class);
        }
    }

    #[test]
    fn classes_round_trip_as_snake_case_strings() {
        for class in [
            FailureClass::Node,
            FailureClass::Provider,
            FailureClass::Execution,
            FailureClass::Dependency,
            FailureClass::Conflict,
        ] {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(serde_json::from_str::<FailureClass>(&json).unwrap(), class);
        }
        assert_eq!(
            serde_json::to_string(&FailureClass::Execution).unwrap(),
            "\"execution\""
        );
    }

    #[test]
    fn every_failure_class_round_trips_through_as_str_and_from_str() {
        for class in FailureClass::ALL {
            let text = class.as_str();
            assert_eq!(FailureClass::from_str(text).unwrap(), class, "{text}");
            assert_eq!(class.to_string(), text);
            assert_eq!(
                serde_json::to_string(&class).unwrap(),
                format!("\"{text}\"")
            );
        }
        assert_eq!(
            "conflict".parse::<FailureClass>().unwrap(),
            FailureClass::Conflict
        );
        assert!("timeout".parse::<FailureClass>().is_err());
    }

    #[test]
    fn every_node_failure_reason_round_trips() {
        for reason in NodeFailureReason::ALL {
            let text = reason.as_str();
            assert_eq!(NodeFailureReason::parse(text).unwrap(), reason, "{text}");
            assert_eq!(NodeFailureReason::from_str(text).unwrap(), reason, "{text}");
            assert_eq!(reason.to_string(), text);
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{text}\"")
            );
            assert_eq!(
                serde_json::from_str::<NodeFailureReason>(&format!("\"{text}\"")).unwrap(),
                reason
            );
        }
        assert!("harness timeout".parse::<NodeFailureReason>().is_err());
        assert!("node".parse::<NodeFailureReason>().is_err());
    }

    #[test]
    fn node_failure_reason_strings_are_fixed_codes() {
        let actual: Vec<&str> = NodeFailureReason::ALL
            .iter()
            .map(|reason| reason.as_str())
            .collect();
        assert_eq!(
            actual,
            [
                "harness_unreachable",
                "container_start",
                "container_wait_timeout",
                "gateway_unavailable",
                "workspace_enospc"
            ]
        );
    }

    #[test]
    fn failure_class_strings_match_docs_api_md() {
        // docs/api.md, "JobView": `node`, `provider`, `execution`, `dependency` or `conflict`.
        let documented = ["node", "provider", "execution", "dependency", "conflict"];
        let actual: Vec<&str> = FailureClass::ALL
            .iter()
            .map(|class| class.as_str())
            .collect();
        assert_eq!(actual, documented);
    }
}
