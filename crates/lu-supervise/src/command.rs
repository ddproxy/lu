use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

/// Environment names a child always receives when the parent has them.
const BASE_ENV: [&str; 3] = ["PATH", "HOME", "LANG"];

/// A program to run without a shell, with an explicit environment.
///
/// The child never inherits the parent environment: it sees only [`BASE_ENV`]
/// (when the parent has them) plus the entries in [`CommandSpec::env`].
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// The program to execute. Resolved by the OS, never by a shell.
    pub program: PathBuf,
    /// Arguments passed verbatim, in order.
    pub args: Vec<OsString>,
    /// Working directory. `None` keeps the parent's.
    pub cwd: Option<PathBuf>,
    /// Extra environment entries added on top of [`BASE_ENV`].
    pub env: Vec<(OsString, OsString)>,
    /// Bytes written to the child's stdin before it is closed. `None` gives it `/dev/null`.
    pub stdin: Option<Vec<u8>>,
}

impl CommandSpec {
    /// A spec that runs `program` with no arguments.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            stdin: None,
        }
    }

    /// Appends one argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets the working directory.
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Adds one environment entry, on top of [`BASE_ENV`].
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Sends these bytes to the child's stdin, then closes it.
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }
}

/// Builds the tokio command: direct exec, own process group, explicit environment.
pub(crate) fn build(spec: &CommandSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd.env_clear();
    for name in BASE_ENV {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    // A harness without a task on stdin sees no terminal input and hits the idle-output timeout.
    if spec.stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Own process group, so a signal reaches the whole tree.
    cmd.process_group(0);
    // A dropped child must not be left running.
    cmd.kill_on_drop(true);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn harness_environment_exposes_no_controller_credentials() {
        assert!(BASE_ENV.iter().all(|name| !name.starts_with("LU_")));
        let spec = CommandSpec::new("/bin/true").env("LU_JOB_SOCKET", "/tmp/job.sock");
        let command = build(&spec);
        for (key, _) in command.as_std().get_envs() {
            let key = key.to_string_lossy();
            assert!(
                key == "LU_JOB_SOCKET" || !key.starts_with("LU_"),
                "credential-like variable reached harness: {key}"
            );
        }
        let values: Vec<_> = command
            .as_std()
            .get_envs()
            .filter(|(key, _)| key.to_string_lossy() == "LU_JOB_SOCKET")
            .collect();
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn harness_environment_is_a_whitelist() {
        let spec = CommandSpec::new("/bin/true")
            .env("LU_JOB_SOCKET", "/tmp/job.sock")
            .env("LU_TOKEN", "explicit-value");
        let command = build(&spec);
        let allowed: Vec<&OsStr> = BASE_ENV
            .iter()
            .map(OsStr::new)
            .chain([OsStr::new("LU_JOB_SOCKET"), OsStr::new("LU_TOKEN")])
            .collect();
        let envs: Vec<_> = command.as_std().get_envs().collect();
        assert!(!envs.is_empty());
        for (key, _) in envs {
            assert!(
                allowed.contains(&key),
                "unexpected inherited variable: {}",
                key.to_string_lossy()
            );
        }
    }
}
