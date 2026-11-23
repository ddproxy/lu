//! Errors from validating or evaluating a schedule.

use std::fmt;

/// A validation or evaluation failure. Every error carries a human-readable path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError {
    path: String,
    reason: String,
}

pub type Result<T> = std::result::Result<T, ScheduleError>;

impl ScheduleError {
    pub fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
        }
    }

    pub fn validation(reason: impl Into<String>) -> Self {
        Self::new("schedule", reason)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

impl std::error::Error for ScheduleError {}
