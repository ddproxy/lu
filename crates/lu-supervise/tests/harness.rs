//! Exact argument vectors and stdin/environment handling for compiled harness profiles.
#![cfg(unix)]

use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use lu_config::ResolvedHarnessProfile;
use lu_supervise::{harness_command, HarnessCommandContext, InvalidArgument, CONTEXT_WARNING};

const AGENT: &str = "[agent]\nname = \"node-a\"\ncontroller_url = \"https://lu.example.net\"\ntoken_file = \"/etc/ting/ting.token\"\nwork_dir = \"/var/lib/ting/work\"\n";

fn profile(body: &str) -> ResolvedHarnessProfile {
    let text = format!("{AGENT}{body}");
    let config = lu_config::Config::parse(&text).expect("a valid profile");
    let mut profiles = config.agent.expect("agent").harness_profiles();
    profiles
        .remove("fake")
        .or_else(|| profiles.remove("opencode"))
        .expect("a resolved profile")
}

fn args(spec: &lu_supervise::CommandSpec) -> Vec<String> {
    spec.args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn context<'a>(
    task_file: &'a Path,
    cwd: &'a Path,
    model: Option<&'a str>,
    provider: Option<&'a str>,
    job_socket: Option<&'a Path>,
    context_warning: bool,
) -> HarnessCommandContext<'a> {
    HarnessCommandContext {
        task_file,
        cwd,
        model,
        provider,
        timeout_secs: 120,
        job_socket,
        context_warning,
    }
}

#[test]
fn the_builtin_profile_keeps_the_legacy_command_line() {
    let profile =
        profile("\n[agent.harness_profiles.opencode]\nserver = \"https://example.net\"\n");
    let spec = harness_command(
        &profile,
        &context(
            Path::new("/jobs/1/task.md"),
            Path::new("/jobs/1"),
            Some("model-x"),
            None,
            None,
            false,
        ),
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
            "Follow the task in the provided file.",
        ]
    );
    assert_eq!(spec.stdin, None);
    assert_eq!(spec.cwd.as_deref(), Some(Path::new("/jobs/1")));
}

#[test]
fn optional_fragments_are_omitted() {
    let profile = profile("\n[agent.harness_profiles.opencode]\n");
    let spec = harness_command(
        &profile,
        &context(
            Path::new("task.md"),
            Path::new("/jobs/1"),
            None,
            None,
            None,
            false,
        ),
    )
    .expect("a valid command");
    assert_eq!(
        args(&spec),
        [
            "run",
            "--auto",
            "-f",
            "task.md",
            "Follow the task in the provided file."
        ]
    );
}

#[test]
fn context_warning_is_an_exact_suffix() {
    let profile = profile("\n[agent.harness_profiles.opencode]\n");
    let spec = harness_command(
        &profile,
        &context(
            Path::new("task.md"),
            Path::new("/jobs/1"),
            None,
            None,
            None,
            true,
        ),
    )
    .expect("a valid command");
    let expected = format!("Follow the task in the provided file.{CONTEXT_WARNING}");
    assert_eq!(
        args(&spec).last().map(String::as_str),
        Some(expected.as_str())
    );
}

#[test]
fn a_custom_profile_expands_every_placeholder_in_order() {
    let body = r#"
[agent.harness_profiles.fake]
argv = ["run", "{cwd}", "{task_file}", "{message}", "{job_socket}", "{result_file}", "{server}", "{provider}", "{model}", "{timeout_secs}", "{server_args}", "{model_args}", "{provider_args}"]
message = "go {task_file}{context_warning}"
wrapper = ["runner", "exec", "--workdir", "{cwd}"]
server = "https://example.net"
server_args = ["--server", "{server}"]
model_args = ["-m", "{model}"]
provider_args = ["--provider", "{provider}"]
result_file = "result.txt"
"#;
    let profile = profile(body);
    let cwd = Path::new("/jobs/9");
    let socket = Path::new("/run/ting/job.sock");
    let spec = harness_command(
        &profile,
        &context(
            Path::new("/jobs/9/task.md"),
            cwd,
            Some("model-x"),
            Some("provider-y"),
            Some(socket),
            true,
        ),
    )
    .expect("a valid command");
    assert_eq!(spec.program.to_str(), Some("runner"));
    assert_eq!(
        args(&spec),
        [
            "exec",
            "--workdir",
            "/jobs/9",
            "run",
            "/jobs/9",
            "/jobs/9/task.md",
            &format!("go /jobs/9/task.md{CONTEXT_WARNING}"),
            "/run/ting/job.sock",
            "/jobs/9/result.txt",
            "https://example.net",
            "provider-y",
            "model-x",
            "120",
            "--server",
            "https://example.net",
            "-m",
            "model-x",
            "--provider",
            "provider-y",
        ]
    );
}

#[test]
fn task_stdin_is_read_and_bounded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let task = dir.path().join("task.md");
    std::fs::write(&task, "task text\n").expect("write task");
    let profile = profile(
        "\n[agent.harness_profiles.fake]\nargv = [\"/bin/cat\"]\nmessage = \"go\"\nstdin = \"task\"\n",
    );
    let spec = harness_command(
        &profile,
        &context(&task, dir.path(), None, None, None, false),
    )
    .expect("a valid command");
    assert_eq!(spec.stdin.as_deref(), Some(&b"task text\n"[..]));
}

#[test]
fn named_environment_variables_are_passed_through() {
    std::env::set_var("HARNESS_TEST_PASSTHROUGH", "value");
    let profile = profile(
        "\n[agent.harness_profiles.fake]\nargv = [\"/bin/true\"]\nmessage = \"go\"\nenv = [\"HARNESS_TEST_PASSTHROUGH\"]\n",
    );
    let spec = harness_command(
        &profile,
        &context(Path::new("t"), Path::new("/w"), None, None, None, false),
    )
    .expect("a valid command");
    let value = spec
        .env
        .iter()
        .find(|(key, _)| key == OsStr::new("HARNESS_TEST_PASSTHROUGH"))
        .expect("the named variable");
    assert_eq!(value.1, "value");
}

#[test]
fn secret_environment_variables_come_from_private_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("value");
    std::fs::write(&file, "file value\n").expect("write secret file");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("private mode");
    let body = format!(
        "\n[agent.harness_profiles.fake]\nargv = [\"/bin/true\"]\nmessage = \"go\"\nsecret_env = {{ HARNESS_SECRET = {:?} }}\n",
        file.to_string_lossy()
    );
    let profile = profile(&body);
    let spec = harness_command(
        &profile,
        &context(Path::new("t"), Path::new("/w"), None, None, None, false),
    )
    .expect("a valid command");
    let value = spec
        .env
        .iter()
        .find(|(key, _)| key == OsStr::new("HARNESS_SECRET"))
        .expect("the named secret variable");
    assert_eq!(value.1, "file value");
}

#[test]
fn unsafe_dynamic_values_are_refused() {
    let builtin = profile("\n[agent.harness_profiles.opencode]\n");
    let ctx = context(Path::new("t"), Path::new("/w"), None, None, None, false);
    let mut leading = builtin.clone();
    leading.message = "-flag".to_owned();
    assert_eq!(
        harness_command(&leading, &ctx).unwrap_err(),
        InvalidArgument::Message
    );
    assert_eq!(
        harness_command(
            &builtin,
            &HarnessCommandContext {
                model: Some("-m"),
                ..ctx
            }
        )
        .unwrap_err(),
        InvalidArgument::Model
    );
    assert_eq!(
        harness_command(
            &builtin,
            &HarnessCommandContext {
                provider: Some("bad\tprovider"),
                ..ctx
            }
        )
        .unwrap_err(),
        InvalidArgument::Provider
    );
    let mut bad_server = builtin;
    bad_server.server = Some("ftp://example.net".to_owned());
    assert_eq!(
        harness_command(&bad_server, &ctx).unwrap_err(),
        InvalidArgument::Server
    );
}
