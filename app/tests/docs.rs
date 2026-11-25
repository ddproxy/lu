//! Documentation checks: README structure, links, CLI names and config examples.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("app crate has a workspace parent")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    fs::read_to_string(root().join(relative)).unwrap_or_else(|e| panic!("read {relative}: {e}"))
}

#[test]
fn readme_status_line_matches_workspace_version() {
    let manifest = read("Cargo.toml");
    let section = manifest
        .split("[workspace.package]")
        .nth(1)
        .expect("workspace package section");
    let line = section
        .lines()
        .find(|line| line.trim_start().starts_with("version"))
        .expect("workspace version");
    let version = line
        .split('"')
        .nth(1)
        .expect("quoted workspace version")
        .to_string();

    let status = read("README.md")
        .lines()
        .find(|line| line.starts_with("Status:"))
        .expect("README status line")
        .to_string();
    assert!(status.contains("stable"), "status line: {status}");
    assert!(status.contains(&version), "status line: {status}");
}

#[test]
fn readme_has_the_required_sections() {
    let readme = read("README.md");
    for heading in [
        "## What and why",
        "## Install",
        "## Quickstart",
        "## How it works",
        "## Configuration",
        "## Operating it",
        "## Security model",
        "## Compatibility",
        "## Limits and non-goals",
        "## Contributing",
        "## Documentation",
        "## License",
    ] {
        assert!(
            readme.lines().any(|line| line.starts_with(heading)),
            "missing {heading}"
        );
    }
    for proof in ["checks: config values", "state: done", "saved <path>"] {
        assert!(readme.contains(proof), "missing expected output {proof}");
    }
}

#[test]
fn documentation_links_resolve() {
    let mut paths = vec![root().join("README.md")];
    for entry in fs::read_dir(root().join("docs")).expect("read docs/") {
        let path = entry.expect("docs entry").path();
        if path.extension().is_some_and(|ext| ext == "md") {
            paths.push(path);
        }
    }

    let mut broken = Vec::new();
    for path in paths {
        let text = fs::read_to_string(&path).expect("read doc");
        let mut chars = text.char_indices().peekable();
        while let Some((index, character)) = chars.next() {
            if character != ']' || chars.peek().map(|(_, c)| *c) != Some('(') {
                continue;
            }
            chars.next();
            let rest = &text[index + 2..];
            let end = rest.find(')').unwrap_or(rest.len());
            let target = rest[..end].trim();
            if target.starts_with("http://")
                || target.starts_with("https://")
                || target.starts_with("mailto:")
                || target.starts_with('#')
            {
                continue;
            }
            let file = target.split('#').next().unwrap_or_default();
            if file.is_empty() {
                continue;
            }
            let resolved = path.parent().expect("doc parent").join(file);
            if !resolved.exists() {
                broken.push(format!("{}: {target}", path.display()));
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

fn help_commands(binary: &str) -> Vec<String> {
    let output = Command::new(binary)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("run {binary} --help: {e}"));
    assert!(output.status.success(), "{binary} --help failed");
    let text = String::from_utf8(output.stdout).expect("utf-8 help");
    let mut commands = Vec::new();
    let mut in_commands = false;
    for line in text.lines() {
        if line.trim_end() == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        let name = line.split_whitespace().next().unwrap_or_default();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            commands.push(name.to_string());
        }
    }
    commands
}

#[test]
fn readme_command_table_names_real_commands() {
    let lu = help_commands(env!("CARGO_BIN_EXE_lu"));
    let ting = help_commands(env!("CARGO_BIN_EXE_ting"));
    let mut checked = 0;

    for line in read("README.md").lines() {
        if !line.starts_with("| `lu ") && !line.starts_with("| `ting") {
            continue;
        }
        let cell = line.split('`').nth(1).expect("command cell");
        let mut words = cell.split_whitespace();
        let binary = words.next().expect("binary name");
        let Some(command) = words.next() else {
            continue;
        };
        // `lu top` is feature-gated; it is verified by the tui test suite instead.
        if command == "top" {
            continue;
        }
        let known = if binary == "lu" {
            lu.iter().any(|name| name == command)
        } else {
            ting.iter().any(|name| name == command)
        };
        assert!(known, "unknown command in README table: {cell}");
        checked += 1;
    }
    assert!(
        checked > 20,
        "expected the full command table, found {checked} rows"
    );
}

fn toml_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.trim() == "```toml" {
            current = Some(String::new());
        } else if line.trim() == "```" {
            if let Some(block) = current.take() {
                if !block.trim().is_empty() {
                    blocks.push(block);
                }
            }
        } else if let Some(block) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    blocks
}

fn check_config(binary: &str, path: &Path) {
    let output = Command::new(binary)
        .arg("check")
        .arg("--config")
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("check {}: {e}", path.display()));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "config check failed for {}: {stdout}{stderr}",
        path.display()
    );
}

#[test]
fn documented_config_examples_validate() {
    let binary = env!("CARGO_BIN_EXE_lu");
    let examples = read("docs/config.md");
    let examples = examples
        .split("## Examples")
        .nth(1)
        .expect("config examples section");
    let blocks = toml_blocks(examples);
    assert_eq!(
        blocks.len(),
        2,
        "expected minimal controller and agent examples"
    );

    let dir = tempfile::tempdir().expect("temp dir");
    for (index, block) in blocks.iter().enumerate() {
        let path = dir.path().join(format!("example-{index}.toml"));
        fs::write(&path, block).expect("write example");
        check_config(binary, &path);
    }
    for relative in ["lu.toml", "contrib/lu-controller.toml", "contrib/ting.toml"] {
        check_config(binary, &root().join(relative));
    }
}

#[test]
fn check_proof_lines_match_the_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_lu"))
        .arg("check")
        .arg("--config")
        .arg(root().join("lu.toml"))
        .output()
        .expect("run lu check");
    assert!(output.status.success(), "lu check failed");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 check output");
    assert!(
        stdout.contains("checks: config values"),
        "missing checks proof line: {stdout}"
    );
    assert!(
        stdout.contains("controller: listen 127.0.0.1:7600, 1 agent(s), 1 operator(s), tls off"),
        "unexpected controller summary: {stdout}"
    );
}

#[test]
fn connection_and_validation_docs_state_the_behaviour() {
    let guide = read("docs/client-guide.md");
    assert!(
        guide.contains("There is no fallback to `[agent]` in `--config`"),
        "client guide must rule out the agent credential fallback"
    );
    for setting in ["LU_URL", "LU_TOKEN_FILE", "LU_CA_FILE", "operator.toml"] {
        assert!(
            guide.contains(setting),
            "client guide must name {setting} when credentials are missing"
        );
    }
    let install = read("docs/install.md");
    let security = read("docs/security.md");
    for text in [&install, &security] {
        assert!(
            text.contains("no `[agent]` fallback") || text.contains("no fallback to `[agent]"),
            "docs must rule out the agent credential fallback"
        );
    }
    assert!(
        install.contains("same value validation") && install.contains("opens no secret files"),
        "install docs must describe the check/serve split"
    );
    let changelog = read("CHANGELOG.md");
    assert!(
        changelog.contains("no `[agent]`-table credential fallback"),
        "changelog must record the operator credential behaviour"
    );
}
