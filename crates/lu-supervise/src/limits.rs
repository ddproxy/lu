use std::time::Duration;

/// Hard limits for one supervised harness run.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Longest the harness may run, measured from spawn.
    pub wall_timeout: Duration,
    /// End the harness when it produces no output for this long.
    /// `None` disables the idle-output timeout.
    pub idle_output_timeout: Option<Duration>,
    /// Once more than this many output bytes were produced, end the group.
    pub max_output_bytes: u64,
    /// Time between the terminate signal and the forced-stop signal.
    pub grace: Duration,
}
