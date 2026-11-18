//! Repositories the controller may read a job's base commit from.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_paths_under_a_root_are_opened() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_repo = root.path().join("inside");
        let outside_repo = outside.path().join("outside");
        git2::Repository::init(&inside_repo).unwrap();
        git2::Repository::init(&outside_repo).unwrap();
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
}
