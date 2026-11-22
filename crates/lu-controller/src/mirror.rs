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
use lu_config::{
    read_private_file, ControllerConfig, ControllerRepo, MirrorConfig, MirrorCredential,
};
use lu_proto::MAX_GIT_REF_BYTES;
use serde::Serialize;

const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const STALE_AFTER_SECS: u64 = 60;
const MAX_MIRROR_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 8 * 1024;
const MAX_HEADS: usize = 20;

/// The only PR refs a job may ask for. Both names are kept as written so the fetch refspec is
/// exactly the one requested; no wildcards are ever derived from job input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullKind {
    Head,
    Merge,
}

impl PullKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Merge => "merge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRef {
    number: String,
    kind: PullKind,
}

impl PullRef {
    fn full_ref(&self) -> String {
        format!("refs/pull/{}/{}", self.number, self.kind.as_str())
    }

    /// The sole refspec sent to git for a PR job. Source and destination are both exact.
    fn fetch_refspec(&self) -> String {
        format!(
            "+refs/pull/{}/{}:refs/lu-pr/{}/{}",
            self.number,
            self.kind.as_str(),
            self.number,
            self.kind.as_str()
        )
    }

    fn local_ref(&self) -> String {
        format!("refs/lu-pr/{}/{}", self.number, self.kind.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedRefKind {
    Branch,
    Tag,
}

impl NamedRefKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "heads",
            Self::Tag => "tags",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RequestedRef {
    Commit(String),
    BranchOrTag {
        lookup: String,
        allow: String,
        kind: NamedRefKind,
    },
    Pull(PullRef),
}

/// Strictly parses the small set of ref shapes jobs may name. Git revision expressions, wildcard
/// refspecs and `refs/*` namespaces other than branches, tags and PR heads are all refused.
fn parse_requested_ref(value: &str) -> Option<RequestedRef> {
    if value.is_empty() || value.len() > MAX_GIT_REF_BYTES {
        return None;
    }
    if is_commit_id(value) {
        return Some(RequestedRef::Commit(value.to_owned()));
    }
    if let Some(pull) = parse_pull_ref(value) {
        return Some(RequestedRef::Pull(pull));
    }
    let (lookup, allow, kind) = if let Some(rest) = value.strip_prefix("refs/heads/") {
        if !valid_ref_name(rest) {
            return None;
        }
        (rest.to_owned(), value.to_owned(), NamedRefKind::Branch)
    } else if let Some(rest) = value.strip_prefix("refs/tags/") {
        if !valid_ref_name(rest) {
            return None;
        }
        (rest.to_owned(), value.to_owned(), NamedRefKind::Tag)
    } else {
        if value.starts_with("refs/") || !valid_ref_name(value) {
            return None;
        }
        (value.to_owned(), value.to_owned(), NamedRefKind::Branch)
    };
    Some(RequestedRef::BranchOrTag {
        lookup,
        allow,
        kind,
    })
}

fn parse_pull_ref(value: &str) -> Option<PullRef> {
    let rest = value.strip_prefix("refs/pull/")?;
    let (number, kind) = rest.rsplit_once('/')?;
    if number.is_empty() || number.len() > 18 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let kind = match kind {
        "head" => PullKind::Head,
        "merge" => PullKind::Merge,
        _ => return None,
    };
    Some(PullRef {
        number: number.to_owned(),
        kind,
    })
}

fn is_commit_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A conservative subset of `git check-ref-format`: names are plain branch/tag names or a single
/// `refs/heads/`/`refs/tags/` prefix, with no revision syntax or path tricks.
fn valid_ref_name(name: &str) -> bool {
    if name.is_empty()
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || matches!(name, "HEAD" | "@")
    {
        return false;
    }
    if name.bytes().any(|b| {
        b < 0x20
            || b == 0x7f
            || b.is_ascii_whitespace()
            || matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' | b'+')
    }) {
        return false;
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return false;
    }
    name.split('/')
        .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

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

    /// `message` keeps its original text; `hint` is appended in parentheses when
    /// the cause is recognisable, and never contains credential material.
    fn with_hint(message: impl Into<String>, hint: Option<&'static str>) -> Self {
        let message = message.into();
        let message = match hint {
            Some(hint) => format!("{message} ({hint})"),
            None => message,
        };
        Self { message }
    }

    /// The ref was not a branch, tag or commit the mirror knows, or the allow-list excluded it.
    pub fn is_unknown_ref(&self) -> bool {
        self.message.starts_with("unknown ref ")
    }

    /// Removes every known credential fragment before the error can leave this module.
    fn scrub(mut self, credential: Option<&Credential>) -> Self {
        if let Some(credential) = credential {
            self.message = credential.scrub(&self.message);
        }
        self
    }
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MirrorError {}

/// A short hint for a recognisable mirror failure cause, or `None`.
///
/// `message` is an error string as reported to the user; credential file contents and
/// URLs with credentials are never part of it, and the hint never quotes them either.
fn hint_for(message: &str) -> Option<&'static str> {
    let git_failure = message.contains("git fetch")
        && (message.contains(" failed")
            || message.contains(" timed out")
            || message.contains("cannot run"));
    if git_failure {
        return Some(
            "the controller runs `git` as its own account; it must be able to run git \
             and reach the remote",
        );
    }
    if message.contains("failed to stat") && message.contains(".gitconfig") {
        return Some("the service account needs a readable HOME");
    }
    None
}

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
    /// `[mirror.credentials.repos]` entries: names or URLs mapped to controller-only token files.
    /// Only paths live here; the file contents are read afresh for each fetch.
    credentials: Vec<MirrorCredential>,
    state: Mutex<HashMap<String, RepoState>>,
    sync_lock: Mutex<()>,
}

impl Mirror {
    pub fn from_config(config: &ControllerConfig) -> Result<Self, String> {
        Self::from_config_with_credentials(config, None)
    }

    /// Builds a mirror with the top-level `[mirror.credentials]` token mappings. Callers that only
    /// hold `ControllerConfig` can attach the mappings later with `with_credentials`.
    pub fn from_config_with_credentials(
        config: &ControllerConfig,
        mirror: Option<&MirrorConfig>,
    ) -> Result<Self, String> {
        let credentials = mirror
            .and_then(|config| config.credentials.as_ref())
            .map(|config| config.repos.clone())
            .unwrap_or_default();
        Self::new_with_credentials(
            config.repos.clone(),
            credentials,
            Path::new(&config.artifacts_dir),
        )
    }

    pub fn new(repos: Vec<ControllerRepo>, artifacts_dir: &Path) -> Result<Self, String> {
        Self::new_with_credentials(repos, Vec::new(), artifacts_dir)
    }

    pub fn new_with_credentials(
        repos: Vec<ControllerRepo>,
        credentials: Vec<MirrorCredential>,
        artifacts_dir: &Path,
    ) -> Result<Self, String> {
        let parent = artifacts_dir
            .parent()
            .ok_or_else(|| "artifacts_dir has no parent for the mirror root".to_owned())?;
        let root = parent.join("mirrors");
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create mirror root {}: {e}", root.display()))?;
        set_mode(&root, 0o700);
        Self::remove_stale_askpass_dirs(&root);
        Ok(Self {
            root,
            repos,
            credentials,
            state: Mutex::new(HashMap::new()),
            sync_lock: Mutex::new(()),
        })
    }

    /// Attaches `[mirror.credentials]` after construction. Existing callers are unchanged when
    /// the top-level config is not threaded through to this constructor.
    pub fn with_credentials(mut self, mirror: &MirrorConfig) -> Self {
        if let Some(credentials) = &mirror.credentials {
            self.credentials = credentials.repos.clone();
        }
        self
    }

    /// Removes askpass helpers left behind when an earlier controller stopped mid-fetch.
    fn remove_stale_askpass_dirs(root: &Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        let own_prefix = format!(".askpass-{}-", std::process::id());
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.starts_with(".askpass-")
                || name.starts_with(&own_prefix)
                || !entry.file_type().is_ok_and(|t| t.is_dir())
            {
                continue;
            }
            let path = entry.path();
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), %e, "could not remove stale askpass directory");
            }
        }
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
        let requested = parse_requested_ref(git_ref)?;
        if !request_allowed(repo, &requested) {
            return None;
        }
        self.resolve_path(&self.mirror_path(name), &requested)
    }

    /// Resolves a job's source ref to the head branch it names, using only the mirrored remote.
    /// Pull-request refs are mapped to the single branch whose tip is the fetched head commit;
    /// payload text is never consulted. Refusing ambiguous or fork-only heads keeps a source-branch
    /// push inside the repository the job came from.
    pub fn source_branch(&self, name: &str, git_ref: &str) -> Result<String, MirrorError> {
        let repo = self
            .repo(name)
            .ok_or_else(|| MirrorError::new(format!("unknown repository {name}")))?;
        let requested =
            parse_requested_ref(git_ref).ok_or_else(|| self.unknown_ref(repo, git_ref))?;
        let commit = self
            .resolve(name, git_ref)
            .ok_or_else(|| self.unknown_ref(repo, git_ref))?;
        let path = self.mirror_path(name);
        let repository = Repository::open(&path).map_err(|e| MirrorError::new(e.to_string()))?;
        let target = git2::Oid::from_str(&commit).map_err(|e| MirrorError::new(e.to_string()))?;
        match requested {
            RequestedRef::BranchOrTag {
                lookup,
                kind: NamedRefKind::Branch,
                ..
            } => {
                let reference = repository
                    .find_reference(&format!("refs/heads/{lookup}"))
                    .map_err(|_| self.unknown_ref(repo, git_ref))?;
                if reference.peel_to_commit().map(|c| c.id()) != Ok(target) {
                    return Err(self.unknown_ref(repo, git_ref));
                }
                Ok(lookup)
            }
            RequestedRef::Pull(_) => {
                let matches: Vec<String> = repository
                    .branches(Some(git2::BranchType::Local))
                    .map_err(|e| MirrorError::new(e.to_string()))?
                    .flatten()
                    .filter_map(|(branch, _)| {
                        let name = branch.name().ok().flatten()?.to_owned();
                        (branch.get().peel_to_commit().map(|c| c.id()).ok()? == target)
                            .then_some(name)
                    })
                    .collect();
                match matches.as_slice() {
                    [only] => Ok(only.clone()),
                    [] => Err(MirrorError::new(
                        "the pull request head has no branch in the source repository".to_owned(),
                    )),
                    _ => Err(MirrorError::new(
                        "the pull request head matches more than one branch in the source repository"
                            .to_owned(),
                    )),
                }
            }
            RequestedRef::Commit(_) | RequestedRef::BranchOrTag { .. } => Err(MirrorError::new(
                "the source ref must name a branch to push back to its source branch".to_owned(),
            )),
        }
    }

    /// Resolves a ref for pinning. Every fetch triggered by a job uses one exact refspec; the
    /// wildcard branch/tag sync is reserved for the operator's explicit sync route.
    pub fn ensure_resolved(&self, name: &str, git_ref: &str) -> Result<String, MirrorError> {
        let repo = self
            .repo(name)
            .ok_or_else(|| MirrorError::new(format!("unknown repository {name}")))?
            .clone();
        let requested =
            parse_requested_ref(git_ref).ok_or_else(|| self.unknown_ref(&repo, git_ref))?;
        if !request_allowed(&repo, &requested) {
            return Err(self.unknown_ref(&repo, git_ref));
        }
        let _guard = self.lock_sync();
        let path = self.mirror_path(name);
        init_mirror_dir(&path, &repo.name)?;
        let stale = self
            .state()
            .get(name)
            .and_then(|state| state.synced_at)
            .map(|at| now_unix().saturating_sub(at) as u64 >= STALE_AFTER_SECS)
            .unwrap_or(true);
        let known = self.resolve_path(&path, &requested);
        match &requested {
            RequestedRef::Pull(pull) => {
                // PR heads move; always fetch the one ref being pinned, even when it is cached.
                self.fetch_pull(&repo, &path, pull)?;
            }
            RequestedRef::BranchOrTag { lookup, kind, .. } => {
                if stale || known.is_none() {
                    let source = format!("refs/{}/{lookup}", kind.as_str());
                    if let Err(error) = self.fetch_exact(&repo, &path, &source, &source) {
                        if *kind == NamedRefKind::Branch {
                            // A name without a prefix may designate a tag; try exactly that tag.
                            let tag = format!("refs/tags/{lookup}");
                            self.fetch_exact(&repo, &path, &tag, &tag)?;
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            RequestedRef::Commit(_) => {}
        }
        self.resolve_path(&path, &requested)
            .ok_or_else(|| self.unknown_ref(&repo, git_ref))
    }

    fn unknown_ref(&self, repo: &ControllerRepo, git_ref: &str) -> MirrorError {
        let shown: String = git_ref.chars().take(128).collect();
        let credential = self.credential_for(repo).ok().flatten();
        MirrorError::new(format!("unknown ref {shown} in repository {}", repo.name))
            .scrub(credential.as_ref())
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

    /// Fetches all branch heads and tags for a configured mirror. This is used only by the
    /// operator sync route, which lists heads; job resolution always fetches exact refspecs.
    fn fetch_all(&self, repo: &ControllerRepo, path: &Path) -> Result<(), MirrorError> {
        let credential = self.credential_for(repo)?;
        let refspecs = [
            "+refs/heads/*:refs/heads/*".to_owned(),
            "+refs/tags/*:refs/tags/*".to_owned(),
        ];
        self.fetch_refspecs(repo, path, &refspecs, credential.as_ref(), true)
    }

    /// Fetches one exact ref (branch or tag) into its matching local namespace. No wildcard is
    /// derived from the job's ref, so a job can never trigger a `+refs/*` fetch.
    fn fetch_exact(
        &self,
        repo: &ControllerRepo,
        path: &Path,
        source: &str,
        destination: &str,
    ) -> Result<(), MirrorError> {
        let credential = self.credential_for(repo)?;
        let refspec = format!("+{source}:{destination}");
        let result = self.fetch_refspecs(repo, path, &[refspec], credential.as_ref(), false);
        self.fetch_outcome(repo, source, result)
    }

    /// Fetches one exact PR ref into a private namespace. No other ref is requested or written.
    fn fetch_pull(
        &self,
        repo: &ControllerRepo,
        path: &Path,
        pull: &PullRef,
    ) -> Result<(), MirrorError> {
        let credential = self.credential_for(repo)?;
        let full_ref = pull.full_ref();
        let result = self.fetch_refspecs(
            repo,
            path,
            &[pull.fetch_refspec()],
            credential.as_ref(),
            false,
        );
        self.fetch_outcome(repo, &full_ref, result)
    }

    /// A remote that simply lacks the requested ref is an unknown-ref (422) condition; transport
    /// and configuration failures stay internal. The converted error never echoes git's stderr.
    fn fetch_outcome(
        &self,
        repo: &ControllerRepo,
        git_ref: &str,
        result: Result<(), MirrorError>,
    ) -> Result<(), MirrorError> {
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let text = error.to_string();
                if text.contains("couldn't find remote ref")
                    || (text.contains("remote ref") && text.contains("does not exist"))
                    || text.contains("no such ref was fetched")
                {
                    Err(self.unknown_ref(repo, git_ref))
                } else {
                    Err(error)
                }
            }
        }
    }

    fn fetch_refspecs(
        &self,
        repo: &ControllerRepo,
        path: &Path,
        refspecs: &[String],
        credential: Option<&Credential>,
        prune: bool,
    ) -> Result<(), MirrorError> {
        let askpass = match credential {
            Some(value) => Some(askpass(value, &self.root).map_err(|e| e.scrub(credential))?),
            None => None,
        };
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(path)
            .arg("fetch")
            .arg("--quiet")
            .arg("--force")
            .arg("--no-recurse-submodules");
        if prune {
            cmd.arg("--prune");
        }
        cmd.arg(&repo.url);
        for refspec in refspecs {
            cmd.arg(refspec);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        harden(&mut cmd, repo, askpass.as_ref());
        let output = run_with_timeout(&mut cmd).map_err(|e| e.scrub(credential))?;
        let stderr = match credential {
            Some(credential) => credential.scrub(&output.stderr),
            None => output.stderr,
        };
        if output.success {
            return Ok(());
        }
        let detail = trim_error(&stderr);
        let base = if detail.is_empty() {
            format!("git fetch for {} failed with {}", repo.name, output.status)
        } else {
            format!(
                "git fetch for {} failed with {}: {}",
                repo.name, output.status, detail
            )
        };
        Err(MirrorError::with_hint(
            format!("fetching mirror for {} failed: {base}", repo.name),
            hint_for(&base),
        )
        .scrub(credential))
    }

    fn sync_locked(&self, repo: &ControllerRepo) -> Result<SyncResult, MirrorError> {
        let path = self.mirror_path(&repo.name);
        init_mirror_dir(&path, &repo.name)?;
        self.fetch_all(repo, &path)?;
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

    fn resolve_path(&self, path: &Path, requested: &RequestedRef) -> Option<String> {
        let repository = Repository::open(path).ok()?;
        match requested {
            RequestedRef::Commit(git_ref) => {
                let oid = Oid::from_str(git_ref).ok()?;
                repository.find_commit(oid).ok()?;
                Some(oid.to_string())
            }
            RequestedRef::BranchOrTag { lookup, .. } => {
                for prefix in ["refs/heads/", "refs/tags/"] {
                    let name = format!("{prefix}{lookup}");
                    if let Ok(reference) = repository.find_reference(&name) {
                        if let Ok(commit) = reference.peel_to_commit() {
                            return Some(commit.id().to_string());
                        }
                    }
                }
                None
            }
            RequestedRef::Pull(pull) => {
                let reference = repository.find_reference(&pull.local_ref()).ok()?;
                reference
                    .peel_to_commit()
                    .ok()
                    .map(|commit| commit.id().to_string())
            }
        }
    }
}

fn init_mirror_dir(path: &Path, name: &str) -> Result<(), MirrorError> {
    if path.exists() {
        return Ok(());
    }
    match Repository::init_bare(path) {
        Ok(_) => {}
        Err(e) => {
            return Err(MirrorError::with_hint(
                format!("creating mirror for {name} failed: {e}"),
                hint_for(&format!("cannot create mirror: {e}")),
            ))
        }
    }
    set_mode(path, 0o700);
    Ok(())
}

fn request_allowed(repo: &ControllerRepo, requested: &RequestedRef) -> bool {
    match requested {
        RequestedRef::Commit(_) => true,
        RequestedRef::BranchOrTag { allow, .. } => ref_allowed(repo, allow),
        RequestedRef::Pull(pull) => ref_allowed(repo, &pull.full_ref()),
    }
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

impl Mirror {
    /// A configured token entry wins over the legacy per-repo `credential_file`. Both are read
    /// from disk here: neither a token nor its file contents are ever placed in a job, artifact or
    /// error message.
    fn credential_for(&self, repo: &ControllerRepo) -> Result<Option<Credential>, MirrorError> {
        if let Some(entry) = self
            .credentials
            .iter()
            .find(|entry| credential_matches(entry, repo))
        {
            return token_credential(entry).map(Some);
        }
        credential(repo)
    }
}

fn credential_matches(entry: &MirrorCredential, repo: &ControllerRepo) -> bool {
    entry.name == repo.name
        || entry.name == repo.url
        || (!normalize_repo_id(&entry.name).is_empty()
            && normalize_repo_id(&entry.name) == normalize_repo_id(&repo.url))
}

fn normalize_repo_id(value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    let value = value.strip_suffix(".git").unwrap_or(value);
    let value = ["https://", "http://", "ssh://", "git://", "git+ssh://"]
        .iter()
        .find_map(|prefix| value.strip_prefix(prefix))
        .unwrap_or(value);
    let value = value.rsplit('@').next().unwrap_or(value);
    value.trim().trim_matches('/').to_ascii_lowercase()
}

fn token_credential(entry: &MirrorCredential) -> Result<Credential, MirrorError> {
    let text = read_private_file(
        &format!("[mirror.credentials] ({}) token_file", entry.name),
        Path::new(&entry.token_file),
        MAX_CREDENTIAL_BYTES,
    )
    .map_err(|e| MirrorError::new(e.to_string()))?;
    credential_from_line(
        &format!("mirror credential {}", entry.name),
        text.trim_end_matches(['\r', '\n']),
    )
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
    credential_from_line(
        &format!("repository {}", repo.name),
        text.trim_end_matches(['\r', '\n']),
    )
    .map(Some)
}

fn credential_from_line(what: &str, line: &str) -> Result<Credential, MirrorError> {
    if line.is_empty() {
        return Err(MirrorError::new(format!("{what} is empty")));
    }
    let (username, password) = match line.split_once(':') {
        Some((user, pass)) => (user.to_owned(), pass.to_owned()),
        None => ("x-access-token".to_owned(), line.to_owned()),
    };
    Ok(Credential {
        username,
        password,
        raw: line.to_owned(),
    })
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
        .env_remove("GIT_SSH_ASKPASS")
        .env_remove("GIT_USERNAME")
        .env_remove("GIT_PASSWORD")
        .env_remove("GIT_TRACE")
        .env_remove("GIT_TRACE_CURL")
        .env_remove("GIT_TRACE_CURL_NO_DATA")
        .env_remove("GIT_CURL_VERBOSE")
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
    let mut child = cmd.spawn().map_err(|e| {
        let base = format!("cannot run git fetch: {e}");
        MirrorError::with_hint(&base, hint_for(&base))
    })?;
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
        None => Err(MirrorError::with_hint(
            format!(
                "git fetch timed out after {} seconds",
                FETCH_TIMEOUT.as_secs()
            ),
            hint_for(&format!(
                "git fetch timed out after {} seconds",
                FETCH_TIMEOUT.as_secs()
            )),
        )),
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

    fn local_repo_with_pull(path: &Path) -> (String, String, String) {
        let (url, first, second) = local_repo(path);
        let repo = Repository::open(path).unwrap();
        repo.reference(
            "refs/pull/42/head",
            Oid::from_str(&first).unwrap(),
            false,
            "pr head",
        )
        .unwrap();
        repo.reference(
            "refs/pull/42/merge",
            Oid::from_str(&second).unwrap(),
            false,
            "pr merge",
        )
        .unwrap();
        (url, first, second)
    }

    fn mirror(repos: Vec<ControllerRepo>) -> (tempfile::TempDir, Mirror) {
        let tmp = tempfile::tempdir().unwrap();
        let artifacts = tmp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let mirror = Mirror::new(repos, &artifacts).unwrap();
        (tmp, mirror)
    }

    #[test]
    fn hint_for_home_config_stat_suggests_readable_home() {
        assert_eq!(
            hint_for("failed to stat '/home/lu/.gitconfig'; class=Config"),
            Some("the service account needs a readable HOME")
        );
    }

    #[test]
    fn hint_for_git_fetch_nonzero_exit_suggests_git_access() {
        assert_eq!(
            hint_for("git fetch for lu failed with exit status: 127: command not found"),
            Some(
                "the controller runs `git` as its own account; it must be able to run git \
                 and reach the remote",
            )
        );
    }

    #[test]
    fn hint_for_cannot_run_git_suggests_git_access() {
        assert_eq!(
            hint_for("cannot run git fetch: No such file or directory (os error 2)"),
            Some(
                "the controller runs `git` as its own account; it must be able to run git \
                 and reach the remote",
            )
        );
    }

    #[test]
    fn hint_for_timed_out_git_fetch_suggests_git_access() {
        assert_eq!(
            hint_for("git fetch timed out after 300 seconds"),
            Some(
                "the controller runs `git` as its own account; it must be able to run git \
                 and reach the remote",
            )
        );
    }

    #[test]
    fn hint_for_other_errors_returns_none() {
        assert_eq!(hint_for("unknown ref main in repository local"), None);
        assert_eq!(hint_for("cannot create mirror: permission denied"), None);
        assert_eq!(hint_for("cannot read mirror: permission denied"), None);
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
    fn stale_askpass_directories_are_removed_on_start() {
        let tmp = tempfile::tempdir().unwrap();
        let artifacts = tmp.path().join("artifacts");
        let root = tmp.path().join("mirrors");
        let stale = root.join(".askpass-0-0");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("pass"), b"secret").unwrap();
        let mirror_data = root.join("repo.git");
        std::fs::create_dir_all(&mirror_data).unwrap();

        let _mirror = Mirror::new(vec![], &artifacts).unwrap();

        assert!(
            !stale.exists(),
            "leftover askpass directory survived startup"
        );
        assert!(mirror_data.exists(), "mirror data must be kept");
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

    #[test]
    fn pull_refs_build_one_exact_fetch_refspec() {
        let head = PullRef {
            number: "42".to_owned(),
            kind: PullKind::Head,
        };
        assert_eq!(head.full_ref(), "refs/pull/42/head");
        assert_eq!(
            head.fetch_refspec(),
            "+refs/pull/42/head:refs/lu-pr/42/head"
        );
        let merge = PullRef {
            number: "7".to_owned(),
            kind: PullKind::Merge,
        };
        assert_eq!(
            merge.fetch_refspec(),
            "+refs/pull/7/merge:refs/lu-pr/7/merge"
        );
        assert!(parse_pull_ref("refs/pull/42/head").is_some());
        assert!(parse_pull_ref("refs/pull/42/merge").is_some());
    }

    #[test]
    fn wildcards_revision_expressions_and_malformed_pr_refs_are_rejected() {
        for bad in [
            "+refs/pull/42/head",
            "+refs/heads/*",
            "refs/pull/*/head",
            "refs/pull/42/*",
            "refs/pull/42/HEAD",
            "refs/pull/42/head/extra",
            "refs/pull//head",
            "refs/pull/abc/head",
            "refs/pull/42/merge?",
            "refs/remotes/origin/main",
            "refs/lu-pr/42/head",
            "main^{tree}",
            "main~1",
            "-oProxyCommand=echo",
            "release/../main",
            "",
        ] {
            assert!(
                parse_requested_ref(bad).is_none(),
                "{bad:?} must not be a job ref"
            );
        }
        assert!(parse_requested_ref("main").is_some());
        assert!(parse_requested_ref("release/1.0").is_some());
        assert!(parse_requested_ref("refs/tags/v1").is_some());
        assert!(parse_requested_ref(&"a".repeat(40)).is_some());
    }

    #[test]
    fn ensure_resolved_fetches_only_the_exact_pr_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, first, second) = local_repo_with_pull(tmp.path());
        let (_dir, mirror) = mirror(vec![repo_config(&url)]);

        assert_eq!(
            mirror
                .ensure_resolved("local", "refs/pull/42/head")
                .unwrap(),
            first
        );
        let path = mirror.mirror_path("local");
        let fetched = Repository::open(&path).unwrap();
        assert_eq!(
            fetched
                .find_reference("refs/lu-pr/42/head")
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
                .to_string(),
            first
        );
        assert!(
            fetched.find_reference("refs/heads/main").is_err(),
            "a PR fetch must not also fetch wildcard branch heads"
        );
        assert_eq!(
            mirror.resolve("local", "refs/pull/42/head"),
            Some(first.clone())
        );
        assert_eq!(
            mirror
                .ensure_resolved("local", "refs/pull/42/merge")
                .unwrap(),
            second
        );
        assert!(Repository::open(&path)
            .unwrap()
            .find_reference("refs/lu-pr/42/merge")
            .is_ok());
    }

    #[test]
    fn invalid_pr_refs_are_rejected_before_any_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, _, _) = local_repo_with_pull(tmp.path());
        let (_dir, mirror) = mirror(vec![repo_config(&url)]);
        for bad in [
            "refs/pull/*/head",
            "refs/pull/42/head/merge",
            "refs/pull/42/merge/../head",
            "+refs/pull/42/head",
            "refs/pull/42/foo",
        ] {
            let error = mirror.ensure_resolved("local", bad).unwrap_err();
            assert!(error.is_unknown_ref(), "{bad}: {error}");
        }
        assert!(
            !mirror.mirror_path("local").exists(),
            "an invalid ref must never start a mirror fetch"
        );
    }

    #[test]
    #[cfg(unix)]
    fn configured_mirror_token_file_is_private_and_never_leaks() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let token_file = tmp.path().join("token");
        std::fs::write(&token_file, "super-secret-token\n").unwrap();
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut config = repo_config(&format!("file://{}", tmp.path().join("missing").display()));
        config.name = "private".to_owned();
        let artifacts = tmp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let mirror = Mirror::new_with_credentials(
            vec![config.clone()],
            vec![MirrorCredential {
                name: "private".to_owned(),
                token_file: token_file.display().to_string(),
            }],
            &artifacts,
        )
        .unwrap();

        let credential = mirror.credential_for(&config).unwrap().unwrap();
        assert_eq!(credential.password, "super-secret-token");
        let error = mirror.sync("private").unwrap_err().to_string();
        assert!(!error.contains("super-secret-token"), "{error}");

        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(mirror.credential_for(&config).is_err());
    }

    #[test]
    fn credentials_match_name_url_and_normalized_id() {
        let repo = repo_config("https://example.net/team/app.git");
        for name in [
            "local",
            "https://example.net/team/app.git",
            "example.net/team/app",
            "ssh://example.net/team/app.git",
        ] {
            let entry = MirrorCredential {
                name: name.to_owned(),
                token_file: "/tmp/lu-token".to_owned(),
            };
            assert!(credential_matches(&entry, &repo), "{name}");
        }
    }
}
