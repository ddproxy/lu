//! Controller-side bare mirrors. Jobs for a configured repository are pinned to a commit that
//! this module resolves from the mirror, so a node no longer has to reach the forge.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use git2::{BranchType, Oid, Repository};
use lu_config::{read_private_file, ControllerConfig, ControllerRepo};
use serde::Serialize;

const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const STALE_AFTER_SECS: u64 = 60;
const MAX_MIRROR_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 8 * 1024;
const MAX_HEADS: usize = 20;

/// A sync error with any credential material already removed.
#[derive(Debug)]
pub struct MirrorError {
    message: String,
}

impl MirrorError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The ref was not a branch, tag or commit the mirror knows, or the allow-list excluded it.
    pub fn is_unknown_ref(&self) -> bool {
        self.message.starts_with("unknown ref ")
    }
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MirrorError {}

/// The result of one sync: the branch heads observed and when it finished.
#[derive(Debug, Clone, Serialize)]
pub struct SyncResult {
    pub name: String,
    pub heads: BTreeMap<String, String>,
    pub synced_at: i64,
}

/// A configured repository and its last successful sync.
#[derive(Debug, Clone, Serialize)]
pub struct RepoInfo {
    pub name: String,
    pub url: String,
    pub synced_at: Option<i64>,
    pub heads: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
struct RepoState {
    synced_at: Option<i64>,
    heads: BTreeMap<String, String>,
}

/// All configured mirrors, rooted at `<artifacts_dir>/../mirrors`.
pub struct Mirror {
    root: PathBuf,
    repos: Vec<ControllerRepo>,
    state: Mutex<HashMap<String, RepoState>>,
    sync_lock: Mutex<()>,
}

impl Mirror {
    pub fn from_config(config: &ControllerConfig) -> Result<Self, String> {
        Self::new(config.repos.clone(), Path::new(&config.artifacts_dir))
    }

    pub fn new(repos: Vec<ControllerRepo>, artifacts_dir: &Path) -> Result<Self, String> {
        let parent = artifacts_dir
            .parent()
            .ok_or_else(|| "artifacts_dir has no parent for the mirror root".to_owned())?;
        let root = parent.join("mirrors");
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create mirror root {}: {e}", root.display()))?;
        set_mode(&root, 0o700);
        Ok(Self {
            root,
            repos,
            state: Mutex::new(HashMap::new()),
            sync_lock: Mutex::new(()),
        })
    }

    pub fn repos(&self) -> &[ControllerRepo] {
        &self.repos
    }

    pub fn repo(&self, name: &str) -> Option<&ControllerRepo> {
        self.repos.iter().find(|repo| repo.name == name)
    }

    pub fn repo_for_url(&self, url: &str) -> Option<&ControllerRepo> {
        self.repos.iter().find(|repo| repo.url == url)
    }

    /// The configured repositories with their latest known sync state.
    pub fn infos(&self) -> Vec<RepoInfo> {
        let state = self.state();
        self.repos
            .iter()
            .map(|repo| {
                let known = state.get(&repo.name).cloned().unwrap_or_default();
                RepoInfo {
                    name: repo.name.clone(),
                    url: repo.url.clone(),
                    synced_at: known.synced_at,
                    heads: known.heads,
                }
            })
            .collect()
    }

    pub fn last_sync_unix(&self, name: &str) -> Option<i64> {
        self.state().get(name).and_then(|state| state.synced_at)
    }

    /// Syncs one configured repository and records its branch heads.
    pub fn sync(&self, name: &str) -> Result<SyncResult, MirrorError> {
        let repo = self
            .repo(name)
            .ok_or_else(|| MirrorError::new(format!("unknown repository {name}")))?
            .clone();
        let _guard = self.lock_sync();
        self.sync_locked(&repo)
    }

    /// Resolves a ref against the mirror without fetching.
    pub fn resolve(&self, name: &str, git_ref: &str) -> Option<String> {
        let repo = self.repo(name)?;
        if !commit_allowed(repo, git_ref) {
            return None;
        }
        self.resolve_path(&self.mirror_path(name), repo, git_ref)
    }

    /// Resolves a ref for pinning: fetches when the mirror is stale or the ref is unknown.
    pub fn ensure_resolved(&self, name: &str, git_ref: &str) -> Result<String, MirrorError> {
        let repo = self
            .repo(name)
            .ok_or_else(|| MirrorError::new(format!("unknown repository {name}")))?
            .clone();
        let _guard = self.lock_sync();
        let stale = self
            .state()
            .get(name)
            .and_then(|state| state.synced_at)
            .map(|at| now_unix().saturating_sub(at) as u64 >= STALE_AFTER_SECS)
            .unwrap_or(true);
        if stale || self.resolve(name, git_ref).is_none() {
            self.sync_locked(&repo)?;
        }
        self.resolve(name, git_ref).ok_or_else(|| {
            MirrorError::new(format!("unknown ref {git_ref} in repository {}", repo.name))
        })
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, RepoState>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_sync(&self) -> MutexGuard<'_, ()> {
        self.sync_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mirror_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.git"))
    }

    fn sync_locked(&self, repo: &ControllerRepo) -> Result<SyncResult, MirrorError> {
        let path = self.mirror_path(&repo.name);
        if !path.exists() {
            Repository::init_bare(&path)
                .map_err(|e| MirrorError::new(format!("cannot create mirror: {e}")))?;
            set_mode(&path, 0o700);
        }
        fetch(repo, &path, &self.root)?;
        let bytes = tree_bytes(&path)?;
        if bytes > MAX_MIRROR_BYTES {
            return Err(MirrorError::new(format!(
                "repository {} is larger than the {} byte mirror cap",
                repo.name, MAX_MIRROR_BYTES
            )));
        }
        let heads = branch_heads(&path, repo)?;
        let synced_at = now_unix();
        self.state().insert(
            repo.name.clone(),
            RepoState {
                synced_at: Some(synced_at),
                heads: heads.clone(),
            },
        );
        Ok(SyncResult {
            name: repo.name.clone(),
            heads,
            synced_at,
        })
    }

    fn resolve_path(&self, path: &Path, repo: &ControllerRepo, git_ref: &str) -> Option<String> {
        let repository = Repository::open(path).ok()?;
        if let Ok(oid) = Oid::from_str(git_ref) {
            if repository.find_commit(oid).is_ok() {
                return Some(oid.to_string());
            }
        }
        if let Ok(branch) = repository.find_branch(git_ref, BranchType::Local) {
            if ref_allowed(repo, git_ref) {
                if let Some(oid) = branch.get().target() {
                    return Some(oid.to_string());
                }
            }
        }
        if ref_allowed(repo, git_ref) {
            if let Ok(object) = repository.revparse_single(&format!("refs/tags/{git_ref}")) {
                if let Ok(commit) = object.peel_to_commit() {
                    return Some(commit.id().to_string());
                }
            }
            if let Ok(object) = repository.revparse_single(git_ref) {
                if let Ok(commit) = object.peel_to_commit() {
                    return Some(commit.id().to_string());
                }
            }
        }
        None
    }
}

fn commit_allowed(repo: &ControllerRepo, git_ref: &str) -> bool {
    if git_ref.len() == 40 && git_ref.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    ref_allowed(repo, git_ref)
}

fn ref_allowed(repo: &ControllerRepo, name: &str) -> bool {
    repo.refs.iter().any(|pattern| glob_match(pattern, name))
}

/// A small glob matcher for the ref allow-list: `*` and `?` only.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 0..p.len() {
        if p[i] == '*' {
            dp[i + 1][0] = dp[i][0];
        }
        for j in 0..t.len() {
            dp[i + 1][j + 1] = match p[i] {
                '*' => dp[i][j + 1] || dp[i + 1][j],
                '?' => dp[i][j],
                c => dp[i][j] && c == t[j],
            };
        }
    }
    dp[p.len()][t.len()]
}

#[derive(Clone)]
struct Credential {
    username: String,
    password: String,
    raw: String,
}

impl Credential {
    fn scrub(&self, text: &str) -> String {
        let mut out = text.replace(&self.raw, "[redacted]");
        if !self.password.is_empty() {
            out = out.replace(&self.password, "[redacted]");
        }
        if !self.username.is_empty() {
            out = out.replace(&self.username, "[redacted]");
        }
        out
    }
}

struct Askpass {
    dir: PathBuf,
}

impl Drop for Askpass {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn credential(repo: &ControllerRepo) -> Result<Option<Credential>, MirrorError> {
    let Some(file) = &repo.credential_file else {
        return Ok(None);
    };
    let text = read_private_file(
        "repository credential_file",
        Path::new(file),
        MAX_CREDENTIAL_BYTES,
    )
    .map_err(|e| MirrorError::new(e.to_string()))?;
    let line = text.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Err(MirrorError::new(format!(
            "repository {} has an empty credential file",
            repo.name
        )));
    }
    let (username, password) = match line.split_once(':') {
        Some((user, pass)) => (user.to_owned(), pass.to_owned()),
        None => ("x-access-token".to_owned(), line.to_owned()),
    };
    Ok(Some(Credential {
        username,
        password,
        raw: line.to_owned(),
    }))
}

fn askpass(credential: &Credential, root: &Path) -> Result<Askpass, MirrorError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = root.join(format!(
        ".askpass-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)
        .map_err(|e| MirrorError::new(format!("cannot create askpass directory: {e}")))?;
    set_mode(&dir, 0o700);
    let askpass = Askpass { dir };
    let script = askpass.dir.join("askpass");
    write_secret(&askpass.dir.join("user"), credential.username.as_bytes())?;
    write_secret(&askpass.dir.join("pass"), credential.password.as_bytes())?;
    write_secret(
        &script,
        b"#!/bin/sh\ncase \"$1\" in\n  *sername*) cat \"$(dirname \"$0\")/user\" ;;\n  *) cat \"$(dirname \"$0\")/pass\" ;;\nesac\n",
    )?;
    set_mode(&script, 0o700);
    Ok(askpass)
}

fn write_secret(path: &Path, bytes: &[u8]) -> Result<(), MirrorError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| MirrorError::new(format!("cannot write askpass helper: {e}")))?;
    file.write_all(bytes)
        .map_err(|e| MirrorError::new(format!("cannot write askpass helper: {e}")))
}

fn fetch(repo: &ControllerRepo, path: &Path, root: &Path) -> Result<(), MirrorError> {
    let credential = credential(repo)?;
    let askpass = match &credential {
        Some(value) => Some(askpass(value, root)?),
        None => None,
    };
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(path)
        .arg("fetch")
        .arg("--quiet")
        .arg("--prune")
        .arg("--force")
        .arg("--no-recurse-submodules")
        .arg(&repo.url)
        .arg("+refs/heads/*:refs/heads/*")
        .arg("+refs/tags/*:refs/tags/*")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    harden(&mut cmd, repo, askpass.as_ref());
    let output = run_with_timeout(&mut cmd)?;
    let stderr = match &credential {
        Some(credential) => credential.scrub(&output.stderr),
        None => output.stderr,
    };
    if output.success {
        return Ok(());
    }
    let detail = trim_error(&stderr);
    if detail.is_empty() {
        Err(MirrorError::new(format!(
            "git fetch for {} failed with {}",
            repo.name, output.status
        )))
    } else {
        Err(MirrorError::new(format!(
            "git fetch for {} failed with {}: {}",
            repo.name, output.status, detail
        )))
    }
}

fn harden(cmd: &mut Command, repo: &ControllerRepo, askpass: Option<&Askpass>) {
    let protocols = if repo.allow_file_urls {
        "file"
    } else {
        "https"
    };
    cmd.env("GIT_ALLOW_PROTOCOL", protocols)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EXTERNAL_DIFF", "")
        .env("GIT_PAGER", "cat")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "http.followRedirects")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env_remove("GIT_SSL_NO_VERIFY")
        .env_remove("GIT_SSH")
        .env_remove("GIT_SSH_COMMAND")
        .env_remove("GIT_PROXY_COMMAND")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    if let Some(askpass) = askpass {
        cmd.env("GIT_ASKPASS", askpass.dir.join("askpass"));
    }
}

struct FetchOutput {
    success: bool,
    status: String,
    stderr: String,
}

fn run_with_timeout(cmd: &mut Command) -> Result<FetchOutput, MirrorError> {
    let mut child = cmd
        .spawn()
        .map_err(|e| MirrorError::new(format!("cannot run git fetch: {e}")))?;
    let stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut out = String::new();
        if let Some(stderr) = stderr {
            let _ = stderr.take(64 * 1024).read_to_string(&mut out);
        }
        out
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < FETCH_TIMEOUT => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(MirrorError::new(format!("cannot wait for git fetch: {e}")));
            }
        }
    };
    let stderr = reader.join().unwrap_or_default();
    match status {
        None => Err(MirrorError::new(format!(
            "git fetch timed out after {} seconds",
            FETCH_TIMEOUT.as_secs()
        ))),
        Some(status) => Ok(FetchOutput {
            success: status.success(),
            status: status.to_string(),
            stderr,
        }),
    }
}

fn trim_error(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(2048) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

fn branch_heads(
    path: &Path,
    repo: &ControllerRepo,
) -> Result<BTreeMap<String, String>, MirrorError> {
    let repository =
        Repository::open(path).map_err(|e| MirrorError::new(format!("cannot open mirror: {e}")))?;
    let mut heads = BTreeMap::new();
    let branches = repository
        .branches(Some(BranchType::Local))
        .map_err(|e| MirrorError::new(format!("cannot list mirror branches: {e}")))?;
    for branch in branches {
        let (branch, _) = branch.map_err(|e| MirrorError::new(format!("bad branch: {e}")))?;
        let Some(name) = branch.name().ok().flatten().map(str::to_owned) else {
            continue;
        };
        if !ref_allowed(repo, &name) {
            continue;
        }
        if let Some(oid) = branch.get().target() {
            heads.insert(name, oid.to_string());
            if heads.len() >= MAX_HEADS {
                break;
            }
        }
    }
    Ok(heads)
}

fn tree_bytes(path: &Path) -> Result<u64, MirrorError> {
    let mut total = 0u64;
    let entries = std::fs::read_dir(path)
        .map_err(|e| MirrorError::new(format!("cannot read mirror: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| MirrorError::new(format!("cannot read mirror: {e}")))?;
        let meta = std::fs::symlink_metadata(entry.path())
            .map_err(|e| MirrorError::new(format!("cannot stat mirror: {e}")))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            total = total.saturating_add(tree_bytes(&entry.path())?);
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A mirrored repository can serve a job's base pack, so an agent needs no forge access.
impl crate::repo::RepoProvider for Mirror {
    fn open(&self, repo: &str) -> Result<Option<Repository>, String> {
        let Some(entry) = self.repo_for_url(repo) else {
            return Ok(None);
        };
        let path = self.mirror_path(&entry.name);
        if !path.exists() {
            return Ok(None);
        }
        Repository::open(&path)
            .map(Some)
            .map_err(|e| format!("cannot open mirror {}: {e}", entry.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::init_main_repo;
    use git2::{IndexAddOption, Signature};
    use std::io::Write;

    fn repo_config(url: &str) -> ControllerRepo {
        ControllerRepo {
            url: url.to_owned(),
            name: "local".to_owned(),
            credential_file: None,
            refs: vec!["*".to_owned()],
            allow_file_urls: true,
        }
    }

    fn local_repo(path: &Path) -> (String, String, String) {
        let repo = init_main_repo(path);
        let file = path.join("file.txt");
        std::fs::write(&file, "one\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = Signature::now("lu", "lu@example.net").unwrap();
        let first = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "first",
                &tree,
                &[],
            )
            .unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        repo.tag_lightweight("v1", first_commit.as_object(), false)
            .unwrap();
        std::fs::write(&file, "two\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let second = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "second",
                &tree,
                &[&first_commit],
            )
            .unwrap();
        (
            format!("file://{}", path.display()),
            first.to_string(),
            second.to_string(),
        )
    }

    fn mirror(repos: Vec<ControllerRepo>) -> (tempfile::TempDir, Mirror) {
        let tmp = tempfile::tempdir().unwrap();
        let artifacts = tmp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let mirror = Mirror::new(repos, &artifacts).unwrap();
        (tmp, mirror)
    }

    #[test]
    fn glob_matches_star_and_question_mark() {
        assert!(glob_match("*", "main"));
        assert!(glob_match("release/*", "release/1.0"));
        assert!(!glob_match("release/*", "main"));
        assert!(glob_match("v?", "v1"));
        assert!(!glob_match("v?", "v12"));
    }

    #[test]
    fn sync_then_resolve_branch_tag_and_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, first, second) = local_repo(tmp.path());
        let (_dir, mirror) = mirror(vec![repo_config(&url)]);
        let sync = mirror.sync("local").unwrap();
        assert_eq!(sync.name, "local");
        assert_eq!(sync.heads.get("main"), Some(&second));
        assert!(sync.synced_at > 0);
        assert_eq!(mirror.resolve("local", "main"), Some(second.clone()));
        assert_eq!(mirror.resolve("local", "v1"), Some(first.clone()));
        assert_eq!(mirror.resolve("local", &first), Some(first.clone()));
        assert_eq!(mirror.resolve("local", "missing"), None);
    }

    #[test]
    fn ensure_resolved_fetches_an_unknown_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, _, second) = local_repo(tmp.path());
        let first = mirror(vec![repo_config(&url)]);
        let (_dir, mirror) = first;
        assert_eq!(mirror.ensure_resolved("local", "main").unwrap(), second);
    }

    #[test]
    fn a_ref_outside_the_allow_list_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, _, _) = local_repo(tmp.path());
        let mut config = repo_config(&url);
        config.refs = vec!["release/*".to_owned()];
        let (_dir, mirror) = mirror(vec![config]);
        mirror.sync("local").unwrap();
        assert_eq!(mirror.resolve("local", "main"), None);
        assert!(mirror.ensure_resolved("local", "main").is_err());
    }

    #[test]
    fn a_credential_never_appears_in_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let credential_path = tmp.path().join("credential");
        std::fs::write(&credential_path, "user:super-secret-token\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&credential_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let mut config = repo_config(&format!("file://{}", tmp.path().join("missing").display()));
        config.credential_file = Some(credential_path.display().to_string());
        let (_dir, mirror) = mirror(vec![config]);
        let error = mirror.sync("local").unwrap_err().to_string();
        assert!(!error.contains("super-secret-token"), "{error}");
        assert!(!error.contains("user:super-secret-token"), "{error}");
    }

    #[test]
    fn mirror_directories_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let artifacts = tmp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let mirror = Mirror::new(vec![], &artifacts).unwrap();
        assert!(mirror.root.ends_with("mirrors"));
        let mut file = std::fs::File::create(mirror.root.join("probe")).unwrap();
        file.write_all(b"x").unwrap();
        assert!(mirror.root.exists());
    }
}
