//! Harness supervision: a child in its own process group, timeouts, idle-output
//! timeout, SIGTERM then SIGKILL, capped output.
//!
//! The child is started directly, never through a shell, with an explicit
//! environment. It runs in its own process group so that stopping it ends the
//! whole tree, not just the direct child. Output is handed over as raw bytes and
//! is never decoded or trimmed here; the caller is responsible for treating it as
//! untrusted.
//!
//! The implementation is Unix-specific: it relies on process groups and signals.
//! On other targets the crate is empty.
//!
//! A process group does not contain a child that calls `setsid` or `setpgid`;
//! callers who need containment run the agent under a service manager that ends
//! the whole control group on stop.
#![cfg(unix)]

mod command;
mod limits;
mod opencode;
mod output;
mod running;

pub use command::CommandSpec;
pub use limits::Limits;
pub use opencode::{opencode_command, InvalidArgument};
pub use output::{OutputChunk, Stream};
pub use running::{Outcome, Running, Supervisor};
