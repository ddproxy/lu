//! The opencode argument vector, including refusals. opencode itself is never run.
#![cfg(unix)]

use std::path::Path;

use lu_supervise::{opencode_command, InvalidArgument};

fn args(spec: &lu_supervise::CommandSpec) -> Vec<String> {
    spec.args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_argument_vector_is_exact() {
    let spec = opencode_command(
        Path::new("/jobs/1/task.md"),
        Some("model-x"),
        Some("https://example.net"),
        "do the thing",
    )
    .expect("a valid command");
    assert_eq!(spec.program.to_str(), Some("opencode"));
    assert_eq!(
        args(&spec),
        [
            "run",
            "--server",
            "https://example.net",
            "--auto",
            "-m",
            "model-x",
            "-f",
            "/jobs/1/task.md",
            "do the thing",
        ]
    );
}

#[test]
fn optional_flags_are_omitted() {
    let spec =
        opencode_command(Path::new("task.md"), None, None, "hello").expect("a valid command");
    assert_eq!(args(&spec), ["run", "--auto", "-f", "task.md", "hello"]);
}

#[test]
fn a_leading_dash_in_the_message_is_refused() {
    assert_eq!(
        opencode_command(Path::new("t"), None, None, "-flag").unwrap_err(),
        InvalidArgument::Message
    );
}

#[test]
fn a_control_character_in_the_message_is_refused() {
    assert_eq!(
        opencode_command(Path::new("t"), None, None, "line\nbreak").unwrap_err(),
        InvalidArgument::Message
    );
    assert_eq!(
        opencode_command(Path::new("t"), None, None, "bell\u{7}").unwrap_err(),
        InvalidArgument::Message
    );
}

#[test]
fn a_leading_dash_in_the_model_is_refused() {
    assert_eq!(
        opencode_command(Path::new("t"), Some("-m"), None, "ok").unwrap_err(),
        InvalidArgument::Model
    );
}

#[test]
fn a_control_character_in_the_model_is_refused() {
    assert_eq!(
        opencode_command(Path::new("t"), Some("bad\tmodel"), None, "ok").unwrap_err(),
        InvalidArgument::Model
    );
}

#[test]
fn an_interior_dash_is_allowed() {
    let spec = opencode_command(Path::new("t"), None, None, "a-b c").expect("a valid command");
    assert_eq!(args(&spec), ["run", "--auto", "-f", "t", "a-b c"]);
}

#[test]
fn a_server_is_accepted() {
    let spec = opencode_command(Path::new("t"), None, Some("https://example.net"), "ok")
        .expect("a valid command");
    assert_eq!(
        args(&spec),
        [
            "run",
            "--server",
            "https://example.net",
            "--auto",
            "-f",
            "t",
            "ok"
        ]
    );
}

#[test]
fn a_server_starting_with_dash_is_refused() {
    assert_eq!(
        opencode_command(Path::new("t"), None, Some("-bad"), "ok").unwrap_err(),
        InvalidArgument::Server
    );
}

#[test]
fn a_server_with_control_character_is_refused() {
    assert_eq!(
        opencode_command(Path::new("t"), None, Some("https://exa\nmple.net"), "ok").unwrap_err(),
        InvalidArgument::Server
    );
}

#[test]
fn a_server_with_whitespace_is_refused() {
    assert_eq!(
        opencode_command(Path::new("t"), None, Some("https://example .net"), "ok").unwrap_err(),
        InvalidArgument::Server
    );
}

#[test]
fn a_server_longer_than_2048_bytes_is_refused() {
    let long = format!("https://{}", "a".repeat(2041));
    assert_eq!(long.len(), 2049);
    assert_eq!(
        opencode_command(Path::new("t"), None, Some(&long), "ok").unwrap_err(),
        InvalidArgument::Server
    );
}

#[test]
fn a_server_must_start_with_http_or_https() {
    assert_eq!(
        opencode_command(Path::new("t"), None, Some("ftp://example.net"), "ok").unwrap_err(),
        InvalidArgument::Server
    );
}
