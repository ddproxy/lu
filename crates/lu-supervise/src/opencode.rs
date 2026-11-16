use std::path::Path;

use crate::command::CommandSpec;

/// Why an opencode command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidArgument {
    /// The message contained a control character or started with `-`.
    Message,
    /// The model contained a control character or started with `-`.
    Model,
    /// The server URL is malformed or unsafe to pass as an option.
    Server,
}

impl std::fmt::Display for InvalidArgument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message => f.write_str("message contains a control character or starts with '-'"),
            Self::Model => f.write_str("model contains a control character or starts with '-'"),
            Self::Server => f.write_str(
                "server starts with '-', contains a control character or whitespace, is longer than 2048 bytes, or does not start with http:// or https://",
            ),
        }
    }
}

impl std::error::Error for InvalidArgument {}

/// Builds `opencode run [--server URL] --auto [-m MODEL] -f TASKFILE MESSAGE`.
///
/// The message, model and server are refused when they contain a control
/// character or start with `-`, so none can be read as another option. The
/// server must also contain no whitespace, be at most 2048 bytes, and start
/// with `http://` or `https://`.
pub fn opencode_command(
    task_file: &Path,
    model: Option<&str>,
    server: Option<&str>,
    message: &str,
) -> Result<CommandSpec, InvalidArgument> {
    if !safe(message) {
        return Err(InvalidArgument::Message);
    }
    if let Some(model) = model {
        if !safe(model) {
            return Err(InvalidArgument::Model);
        }
    }
    if let Some(server) = server {
        if !safe_server(server) {
            return Err(InvalidArgument::Server);
        }
    }

    let mut spec = CommandSpec::new("opencode").arg("run");
    if let Some(server) = server {
        spec = spec.arg("--server").arg(server);
    }
    spec = spec.arg("--auto");
    if let Some(model) = model {
        spec = spec.arg("-m").arg(model);
    }
    spec = spec
        .arg("-f")
        .arg(task_file.as_os_str().to_owned())
        .arg(message);
    Ok(spec)
}

/// Rejects control characters and a leading `-`, which opencode would read as an option.
fn safe(argument: &str) -> bool {
    !argument.starts_with('-') && !argument.chars().any(char::is_control)
}

/// Rejects unsafe or malformed server values.
fn safe_server(server: &str) -> bool {
    if server.starts_with('-') || server.len() > 2048 {
        return false;
    }
    if server
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return false;
    }
    server.starts_with("http://") || server.starts_with("https://")
}
