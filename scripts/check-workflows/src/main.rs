//! Checks workflow YAML, SHA pinning and least-privilege job permissions.
//! Run from anywhere with: cargo run --manifest-path scripts/check-workflows/Cargo.toml

use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use serde_yaml::Value;

type Permissions = &'static [(&'static str, &'static str)];

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("scripts/check-workflows under the repository root");
    let mut problems = Vec::new();

    let mut workflows: Vec<PathBuf> = fs::read_dir(root.join(".github/workflows"))
        .expect("read .github/workflows")
        .map(|entry| entry.expect("read workflow entry").path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yml" | "yaml")
            )
        })
        .collect();
    workflows.sort();

    for path in workflows {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).expect("read workflow");
        let doc: Value = match serde_yaml::from_str(&text) {
            Ok(doc) => doc,
            Err(err) => {
                problems.push(format!("{name}: invalid YAML: {err}"));
                continue;
            }
        };
        check_top_level_permissions(&name, &doc, &mut problems);
        check_pinned_uses(&name, &text, &mut problems);
        check_job_permissions(&name, &doc, &mut problems);
        check_release_triggers(&name, &doc, &text, &mut problems);
    }

    check_dependabot(root, &mut problems);

    if problems.is_empty() {
        println!("workflow checks passed");
        ExitCode::SUCCESS
    } else {
        for problem in problems {
            eprintln!("FAIL: {problem}");
        }
        ExitCode::FAILURE
    }
}

fn check_top_level_permissions(name: &str, doc: &Value, problems: &mut Vec<String>) {
    match doc.get("permissions") {
        Some(Value::Mapping(mapping)) if mapping.is_empty() => {}
        Some(_) => problems.push(format!("{name}: top-level permissions must be {{}}")),
        None => problems.push(format!("{name}: missing top-level permissions: {{}}")),
    }
}

/// Dev builds must run release.yml on the tag ref, never as a reusable call.
fn check_release_triggers(name: &str, doc: &Value, text: &str, problems: &mut Vec<String>) {
    if text.lines().any(|line| {
        let line = line.trim();
        line.starts_with("workflow_call:") || line.starts_with("uses: ./.github/workflows/")
    }) {
        problems.push(format!(
            "{name}: reusable workflows change the signing identity; dispatch on the tag ref"
        ));
    }
    let on = doc.get("on").or_else(|| doc.get(Value::Bool(true)));
    if name == "release.yml" {
        let tags: Vec<&str> = on
            .and_then(|on| on.get("push"))
            .and_then(|push| push.get("tags"))
            .and_then(Value::as_sequence)
            .map(|tags| tags.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !tags.contains(&"v*") || !tags.contains(&"!v*-dev.*") {
            problems.push(format!(
                "{name}: push tags must be v* and exclude dev tags with !v*-dev.*"
            ));
        }
        if on.and_then(|on| on.get("workflow_dispatch")).is_none() {
            problems.push(format!("{name}: missing workflow_dispatch trigger"));
        }
    }
    if name == "dev-release.yml" && !text.contains("gh workflow run release.yml") {
        problems.push(format!(
            "{name}: must start release.yml with gh workflow run"
        ));
    }
}

fn check_pinned_uses(name: &str, text: &str, problems: &mut Vec<String>) {
    for (line_number, line) in text.lines().enumerate() {
        let Some(position) = line.find("uses:") else {
            continue;
        };
        let rest = line[position + 5..].trim();
        let mut fields = rest.splitn(2, '#');
        let action = fields.next().unwrap_or_default().trim();
        let version = fields.next().map(str::trim);
        let pinned = action.rsplit_once('@').is_some_and(|(repo, sha)| {
            repo.split_once('/').is_some_and(|(owner, name)| {
                !owner.is_empty()
                    && !name.is_empty()
                    && !owner.contains(char::is_whitespace)
                    && !name.contains(char::is_whitespace)
            }) && sha.len() == 40
                && sha
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        });
        let has_version = version.is_some_and(|version| {
            version.strip_prefix('v').is_some_and(|digits| {
                !digits.is_empty()
                    && digits.split('.').all(|part| {
                        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
        });
        if !pinned || !has_version {
            problems.push(format!(
                "{name}:{}: expected owner/repo@<40-hex SHA> # vX.Y.Z, got {rest:?}",
                line_number + 1
            ));
        }
    }
}

fn check_job_permissions(name: &str, doc: &Value, problems: &mut Vec<String>) {
    let Some(Value::Mapping(jobs)) = doc.get("jobs") else {
        problems.push(format!("{name}: jobs must be a mapping"));
        return;
    };
    for (job_key, job) in jobs {
        let job_name = job_key.as_str().unwrap_or_default();
        let Some(expected) = expected_permissions(name, job_name) else {
            problems.push(format!("{name}: no permission policy for job {job_name:?}"));
            continue;
        };
        let Some(Value::Mapping(actual)) = job.get("permissions") else {
            problems.push(format!("{name}/{job_name}: missing job permissions"));
            continue;
        };
        let actual: Vec<(String, String)> = actual
            .iter()
            .map(|(key, value)| {
                (
                    key.as_str().unwrap_or_default().to_owned(),
                    value.as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = expected
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        if actual != expected {
            problems.push(format!(
                "{name}/{job_name}: permissions {actual:?}, expected {expected:?}"
            ));
        }
    }
}

fn expected_permissions(workflow: &str, job: &str) -> Option<Permissions> {
    const READ: Permissions = &[("contents", "read")];
    Some(match (workflow, job) {
        ("ci.yml", "test" | "musl" | "compat") => READ,
        ("dev-release.yml", "tag") => &[("actions", "write"), ("contents", "write")],
        ("pages.yml", "build") => READ,
        ("pages.yml", "deploy") => &[("pages", "write"), ("id-token", "write")],
        ("codeql.yml", "analyze") => &[
            ("actions", "read"),
            ("contents", "read"),
            ("security-events", "write"),
        ],
        ("scorecard.yml", "analysis") => &[
            ("contents", "read"),
            ("id-token", "write"),
            ("security-events", "write"),
        ],
        ("dependency-review.yml", "dependency-review") => READ,
        ("audit.yml", "cargo-deny") => READ,
        ("fuzz.yml", "fuzz") => READ,
        ("release.yml", "create") => &[("contents", "write")],
        ("release.yml", "build" | "sbom") => READ,
        ("release.yml", "prune") => &[("contents", "write")],
        ("release.yml", "publish") => &[
            ("attestations", "write"),
            ("contents", "write"),
            ("id-token", "write"),
        ],
        _ => return None,
    })
}

fn check_dependabot(root: &Path, problems: &mut Vec<String>) {
    let path = root.join(".github/dependabot.yml");
    let text = fs::read_to_string(&path).expect("read .github/dependabot.yml");
    let doc: Value = match serde_yaml::from_str(&text) {
        Ok(doc) => doc,
        Err(err) => {
            problems.push(format!("dependabot.yml: invalid YAML: {err}"));
            return;
        }
    };
    if doc.get("version").and_then(Value::as_i64) != Some(2) {
        problems.push("dependabot.yml: version must be 2".into());
    }
    let Some(Value::Sequence(updates)) = doc.get("updates") else {
        problems.push("dependabot.yml: updates must be a sequence".into());
        return;
    };
    let mut ecosystems = Vec::new();
    for update in updates {
        let Some(ecosystem) = update.get("package-ecosystem").and_then(Value::as_str) else {
            problems.push("dependabot.yml: missing package-ecosystem".into());
            continue;
        };
        ecosystems.push(ecosystem.to_owned());
        if update
            .get("schedule")
            .and_then(|s| s.get("interval"))
            .and_then(Value::as_str)
            != Some("weekly")
        {
            problems.push(format!(
                "dependabot.yml/{ecosystem}: schedule must be weekly"
            ));
        }
        if update
            .get("open-pull-requests-limit")
            .and_then(Value::as_i64)
            != Some(5)
        {
            problems.push(format!(
                "dependabot.yml/{ecosystem}: open-pull-requests-limit must be 5"
            ));
        }
        let types = update
            .get("groups")
            .and_then(|groups| groups.get("minor-and-patch"))
            .and_then(|group| group.get("update-types"))
            .and_then(Value::as_sequence);
        let has_types = types.is_some_and(|types| {
            ["minor", "patch"]
                .iter()
                .all(|kind| types.iter().any(|value| value.as_str() == Some(*kind)))
        });
        if !has_types {
            problems.push(format!(
                "dependabot.yml/{ecosystem}: group minor-and-patch needs minor and patch"
            ));
        }
    }
    ecosystems.sort();
    if ecosystems != ["cargo", "github-actions"] {
        problems.push(format!(
            "dependabot.yml: ecosystems {ecosystems:?}, expected cargo and github-actions"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, text: &str) -> Vec<String> {
        let doc: Value = serde_yaml::from_str(text).unwrap();
        let mut problems = Vec::new();
        check_release_triggers(name, &doc, text, &mut problems);
        problems
    }

    const RELEASE: &str = "on:\n  push:\n    tags: [\"v*\", \"!v*-dev.*\"]\n  workflow_dispatch:\n";

    #[test]
    fn accepts_dispatch_release() {
        assert!(run("release.yml", RELEASE).is_empty());
    }

    #[test]
    fn rejects_reusable_call() {
        let text = format!("{RELEASE}  workflow_call:\n");
        assert!(!run("release.yml", &text).is_empty());
        let caller = "jobs:\n  r:\n    uses: ./.github/workflows/release.yml\n";
        assert!(!run("dev-release.yml", caller).is_empty());
    }

    #[test]
    fn rejects_dev_tags_on_push() {
        let text = "on:\n  push:\n    tags: [\"v*\"]\n  workflow_dispatch:\n";
        assert!(!run("release.yml", text).is_empty());
    }

    #[test]
    fn dev_release_policy_row() {
        let want: Permissions = &[("actions", "write"), ("contents", "write")];
        assert_eq!(expected_permissions("dev-release.yml", "tag"), Some(want));
        assert_eq!(expected_permissions("dev-release.yml", "release"), None);
        assert!(expected_permissions("release.yml", "prune").is_some());
    }
}
