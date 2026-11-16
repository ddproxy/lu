use std::path::Path;

use crate::command::CommandSpec;

/// Why an opencode command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidArgument {
    /// The message contained a control character or started with `-`.
    Message,
    /// The model contained a control character or started with `-`.
    Model,
}

impl std::fmt::Display for InvalidArgument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message => f.write_str("message contains a control character or starts with '-'"),
            Self::Model => f.write_str("model contains a control character or starts with '-'"),
        }
    }
}

impl std::error::Error for InvalidArgument {}

/// Builds `opencode run [--server URL] --auto [-m MODEL] -f TASKFILE MESSAGE`.
///
/// The message and the model are refused when they contain a control character
/// or start with `-`, so neither can be read as another option.
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
