//! Repositories the controller may read a job's base commit from.

use serde_json::Value;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Finds the repository named by a job's `repo` field. `None` means "not served here".
pub trait RepoProvider: Send + Sync {
    fn open(&self, repo: &str) -> Result<Option<git2::Repository>, String>;
}

/// Serves absolute paths beneath a fixed list of roots. Roots and candidates are canonicalised
/// before the prefix check so a symlink cannot escape the allow-list.
#[derive(Debug, Clone)]
pub struct LocalPathProvider {
    roots: Vec<PathBuf>,
}

impl LocalPathProvider {
    pub fn new(roots: &[String]) -> Result<Self, String> {
        let mut canonical = Vec::with_capacity(roots.len());
        for root in roots {
            let path = fs::canonicalize(root)
                .map_err(|e| format!("cannot open local_repo_roots entry {root:?}: {e}"))?;
            if !path.is_dir() {
                return Err(format!(
                    "local_repo_roots entry {root:?} is not a directory"
                ));
            }
            canonical.push(path);
        }
        Ok(Self { roots: canonical })
    }
}

impl RepoProvider for LocalPathProvider {
    fn open(&self, repo: &str) -> Result<Option<git2::Repository>, String> {
        let path = Path::new(repo);
        if !path.is_absolute() {
            return Ok(None);
        }
        let Ok(candidate) = fs::canonicalize(path) else {
            return Ok(None);
        };
        if !self.roots.iter().any(|root| candidate.starts_with(root)) {
            return Ok(None);
        }
        git2::Repository::open(&candidate)
            .map(Some)
            .map_err(|e| format!("cannot open repository {}: {e}", candidate.display()))
    }
}

/// A webhook pointed at a pull request whose head lives in a different repository. The error is
/// deliberately value-free: untrusted webhook text must never reach an error, log or artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkNotAllowed;

impl fmt::Display for ForkNotAllowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("pull request head comes from a fork and this rule does not allow forks")
    }
}

impl std::error::Error for ForkNotAllowed {}

/// A pure check over webhook-style pointers. Only fields a forge commonly supplies are read; when
/// none are present the delivery is not treated as a fork, since there is nothing to compare.
pub fn fork_detected(body: &Value, base_repo: &str) -> bool {
    const BASE_POINTERS: [&str; 3] = [
        "/repository/full_name",
        "/repository/url",
        "/repository/name",
    ];
    const HEAD_POINTERS: [&str; 4] = [
        "/pull_request/head/repo/full_name",
        "/pull_request/head/repository/full_name",
        "/head/repo/full_name",
        "/head_repository/full_name",
    ];

    let mut bases: Vec<String> = BASE_POINTERS
        .iter()
        .filter_map(|pointer| body.pointer(pointer).and_then(as_text))
        .filter_map(|text| repo_identity(&text))
        .collect();
    if let Some(base) = repo_identity(base_repo) {
        bases.push(base);
    }
    if bases.is_empty() {
        return false;
    }
    HEAD_POINTERS
        .iter()
        .filter_map(|pointer| body.pointer(pointer).and_then(as_text))
        .filter_map(|text| repo_identity(&text))
        .any(|head| !bases.iter().any(|base| same_repo(base, &head)))
}

/// Refuses a fork unless the intake rule explicitly allowed it. Intake can call this before it
/// creates a draft or queued job; no payload value is returned or logged on refusal.
pub fn fork_refusal(
    body: &Value,
    base_repo: &str,
    allow_forks: bool,
) -> Result<(), ForkNotAllowed> {
    if allow_forks || !fork_detected(body, base_repo) {
        return Ok(());
    }
    Err(ForkNotAllowed)
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

/// Reduces a forge name or URL to `owner/repo`, ignoring scheme, credentials, host and `.git`.
fn repo_identity(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let without_user = trimmed.rsplit('@').next().unwrap_or(trimmed);
    let without_scheme = ["https://", "http://", "ssh://", "git://", "git+ssh://"]
        .iter()
        .find_map(|prefix| without_user.strip_prefix(prefix))
        .unwrap_or(without_user);
    let after_colon = match without_scheme.rsplit_once(':') {
        Some((_, rest)) if rest.contains('/') => rest,
        _ => without_scheme,
    };
    let path = match after_colon.split_once('/') {
        Some((host, rest)) if host.contains('.') || host.contains(':') => rest,
        _ => after_colon,
    };
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let first = parts.len().saturating_sub(2);
    if first >= parts.len() {
        return None;
    }
    Some(parts[first..].join("/").to_ascii_lowercase())
}

/// A short name such as `r` can match `org/r`; two fully qualified names must match exactly.
fn same_repo(base: &str, head: &str) -> bool {
    base == head
        || (!base.contains('/') && head.ends_with(&format!("/{base}")))
        || (!head.contains('/') && base.ends_with(&format!("/{head}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_paths_under_a_root_are_opened() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_repo = root.path().join("inside");
        let outside_repo = outside.path().join("outside");
        crate::test_support::init_main_repo(&inside_repo);
        crate::test_support::init_main_repo(&outside_repo);
        let provider = LocalPathProvider::new(&[root.path().display().to_string()]).unwrap();
        assert!(provider
            .open(&inside_repo.display().to_string())
            .unwrap()
            .is_some());
        assert!(provider
            .open(&outside_repo.display().to_string())
            .unwrap()
            .is_none());
        assert!(provider.open("relative/path").unwrap().is_none());
    }

    #[test]
    fn forks_are_refused_unless_the_rule_allowed_them() {
        let fork = json!({
            "repository": {"full_name": "org/base", "url": "https://example.net/org/base.git"},
            "pull_request": {"head": {"repo": {"full_name": "contributor/base"}}}
        });
        assert!(fork_detected(&fork, "https://example.net/org/base.git"));
        assert_eq!(
            fork_refusal(&fork, "https://example.net/org/base.git", false),
            Err(ForkNotAllowed)
        );
        assert!(fork_refusal(&fork, "https://example.net/org/base.git", true).is_ok());
        let refusal = fork_refusal(&fork, "https://example.net/org/base.git", false)
            .unwrap_err()
            .to_string();
        assert!(!refusal.contains("contributor"), "{refusal}");
        assert!(!refusal.contains("base"), "{refusal}");
    }

    #[test]
    fn same_repository_pointers_are_not_forks() {
        let same = json!({
            "repository": {"full_name": "org/base"},
            "head_repository": {"full_name": "org/base"}
        });
        assert!(!fork_detected(&same, "https://example.net/org/base.git"));
        assert!(fork_refusal(&same, "https://example.net/org/base.git", false).is_ok());
        let no_pointers = json!({"action": "opened"});
        assert!(!fork_detected(
            &no_pointers,
            "https://example.net/org/base.git"
        ));
    }
}
