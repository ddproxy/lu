//! Controller-side publishing.
//!
//! Only the controller ever sees a push token. The token file path lives in `[publish]`; its
//! contents are read for one push and never stored in a job, view, log, artifact or audit record.
//! Every push uses exactly one exact refspec: no force, no tag, no delete, no wildcard.
//!
//! Two independent decisions gate a push. The *target* decision resolves redirects and default-deny
//! for source remotes; the *source-branch opt-in* additionally requires a config flag, the spec
//! target, the origin's allowance, an operator approval and a protected-name check. A failed gate
//! never pushes to a source branch.
//!
//! The pure helpers in this module are unit-tested; the git work is exercised against a local
//! bare repository in `tests/publish.rs`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use git2::build::CheckoutBuilder;
use git2::{
    ApplyLocation, Cred, Diff, Direction, Oid, PushOptions, RemoteCallbacks, Repository, Signature,
};
use lu_config::PublishConfig;
use lu_proto::{printable, PublishGateRecord, PublishRecord, PublishSpec, PublishTarget};
use lu_store::JobRecord;

use crate::state::AppState;

/// Why a publish was refused. All variants are safe to show to an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    NotConfigured,
    NoSpec,
    Denied(String),
    Invalid(String),
    State(String),
    Git(String),
    RateLimited(u64),
}

impl PublishError {
    /// Removes a credential from any text that came from a git or config error.
    pub fn scrub(mut self, token: &str) -> Self {
        if token.is_empty() {
            return self;
        }
        let scrub = |text: &mut String| {
            *text = text.replace(token, "[redacted]");
        };
        match &mut self {
            Self::Git(text) | Self::Invalid(text) | Self::State(text) | Self::Denied(text) => {
                scrub(text)
            }
            Self::NotConfigured | Self::NoSpec | Self::RateLimited(_) => {}
        }
        self
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::NoSpec => "no_spec",
            Self::Denied(_) => "denied",
            Self::Invalid(_) => "invalid",
            Self::State(_) => "state",
            Self::Git(_) => "git",
            Self::RateLimited(_) => "rate_limited",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::NotConfigured => "publishing is not configured",
            Self::NoSpec => "job has no publish step",
            Self::Denied(text) | Self::Invalid(text) | Self::State(text) | Self::Git(text) => text,
            // The HTTP response carries Retry-After; keep the message free of remote names and
            // token text.
            Self::RateLimited(_) => "publish remote push budget is full",
        }
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for PublishError {}

/// The resolved destination of one publish attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveTarget {
    pub remote: String,
    pub target: PublishTarget,
    pub branch: String,
    pub source_branch: Option<String>,
}

/// Normalises a source repository URL to `host/owner/name` for redirect matching. Paths without a
/// scheme (operator-configured local repositories) keep their last three components under `local/`.
pub fn source_key(repo: &str) -> Option<String> {
    let value = repo.trim();
    if value.is_empty() || value.len() > 2048 || value.contains(char::is_whitespace) {
        return None;
    }
    let (host, path) = match value.split_once("://") {
        Some(("https" | "http" | "ssh" | "git", rest)) => {
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit('@').next()?;
            (host.to_owned(), path.to_owned())
        }
        Some(_) => return None,
        None => ("local".to_owned(), value.trim_start_matches('/').to_owned()),
    };
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(&path);
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let owner = parts[parts.len() - 2];
    let name = parts[parts.len() - 1];
    if !valid_component(&host) || !valid_component(owner) || !valid_component(name) {
        return None;
    }
    Some(format!("{host}/{owner}/{name}"))
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains("..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'+'))
}

/// A tiny, allocation-free glob matcher: `*` matches any run of characters, other bytes match
/// literally. The config grammar bounds patterns and values, so this cannot blow up.
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut after_star = 0usize;
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'*' || pattern[p] == value[v]) {
            if pattern[p] == b'*' {
                star = Some(p);
                after_star = v;
                p += 1;
            } else {
                p += 1;
                v += 1;
            }
        } else if let Some(index) = star {
            p = index + 1;
            after_star += 1;
            v = after_star;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Chooses the most specific redirect: an exact match first, then the pattern with the most
/// literal bytes, then lexical order for a deterministic result.
pub fn redirect_for<'a>(config: &'a PublishConfig, source: &str) -> Option<(&'a str, &'a str)> {
    if let Some((pattern, remote)) = config.redirects.get_key_value(source) {
        return Some((pattern.as_str(), remote.as_str()));
    }
    config
        .redirects
        .iter()
        .filter(|(pattern, _)| glob_matches(pattern, source))
        .max_by(|(left, _), (right, _)| {
            let left_score = (
                left.as_str() == source,
                left.bytes().filter(|b| *b != b'*').count(),
                left.as_str(),
            );
            let right_score = (
                right.as_str() == source,
                right.bytes().filter(|b| *b != b'*').count(),
                right.as_str(),
            );
            left_score.cmp(&right_score)
        })
        .map(|(pattern, remote)| (pattern.as_str(), remote.as_str()))
}

/// The default target when a source remote is itself configured as a publish remote.
pub fn source_remote<'a>(config: &'a PublishConfig, source: &str) -> Option<&'a str> {
    config
        .remotes
        .iter()
        .find(|(_, remote)| source_key(&remote.url).as_deref() == Some(source))
        .map(|(name, _)| name.as_str())
}

/// Built-in plus configured protected branch names.
pub fn is_protected(config: &PublishConfig, branch: &str) -> bool {
    ["main", "master", "work"].contains(&branch)
        || glob_matches("release*", branch)
        || config
            .protected_patterns
            .iter()
            .any(|pattern| glob_matches(pattern, branch))
}

/// Substitutes `<key>` in a branch template. The key is an idempotency key or job id, both of which
/// were validated when the job was created.
pub fn branch_from_template(template: &str, key: &str) -> String {
    template.replace("<key>", key)
}

fn prefix_allowed(remote: &lu_config::PublishRemote, branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with("refs/")
        && !branch.contains("..")
        && remote
            .branch_prefixes
            .iter()
            .any(|prefix| branch.starts_with(prefix) && branch.len() > prefix.len())
}

/// Resolves redirects, explicit remote selection and snapshot branch names. Source-branch pushes
/// return a `source_branch` of `None` here: the branch name is only available from the mirror after
/// the operator and origin checks have passed.
pub fn effective_target(
    config: &PublishConfig,
    spec: &PublishSpec,
    source_repo: &str,
    key: &str,
    approved: bool,
) -> Result<EffectiveTarget, PublishError> {
    let source = source_key(source_repo).ok_or_else(|| {
        PublishError::Invalid("source.repo is not a recognised repository URL".to_owned())
    })?;
    let remote_name = match &spec.remote {
        Some(name) => {
            if !config.remotes.contains_key(name) {
                return Err(PublishError::Denied(format!(
                    "unknown publish remote {name:?}"
                )));
            }
            name.clone()
        }
        None => match redirect_for(config, &source) {
            Some((_, name)) => name.to_owned(),
            None => source_remote(config, &source)
                .ok_or_else(|| {
                    PublishError::Denied(
                        "source remote is not a configured publish remote and has no redirect"
                            .to_owned(),
                    )
                })?
                .to_owned(),
        },
    };
    let remote = &config.remotes[&remote_name];
    match spec.target {
        PublishTarget::Snapshot => {
            let template = spec
                .branch
                .as_deref()
                .unwrap_or(lu_proto::DEFAULT_PUBLISH_BRANCH);
            let branch = branch_from_template(template, key);
            if !prefix_allowed(remote, &branch) || is_protected(config, &branch) {
                return Err(PublishError::Denied(format!(
                    "branch {branch:?} is not allowed on remote {remote_name:?}"
                )));
            }
            Ok(EffectiveTarget {
                remote: remote_name,
                target: PublishTarget::Snapshot,
                branch,
                source_branch: None,
            })
        }
        PublishTarget::SourceBranch => {
            if !remote.allow_source_branch_push {
                return Err(PublishError::Denied(format!(
                    "remote {remote_name:?} does not allow source-branch pushes"
                )));
            }
            if !spec.allow_source_branch {
                return Err(PublishError::Denied(
                    "the job's origin did not allow source-branch pushes".to_owned(),
                ));
            }
            if !approved {
                return Err(PublishError::Denied(
                    "source-branch pushes require operator submission or draft approval".to_owned(),
                ));
            }
            Ok(EffectiveTarget {
                remote: remote_name,
                target: PublishTarget::SourceBranch,
                branch: String::new(),
                source_branch: None,
            })
        }
    }
}

/// Checks the mirror-resolved branch name and fills it into an already-approved source-branch
/// target. The name cannot come from spec text at this point.
pub fn with_source_branch(
    config: &PublishConfig,
    remote_name: &str,
    target: &mut EffectiveTarget,
    branch: String,
) -> Result<(), PublishError> {
    if target.target != PublishTarget::SourceBranch {
        return Err(PublishError::State(
            "source branch is only set for source-branch targets".to_owned(),
        ));
    }
    if branch.is_empty() || branch.starts_with("refs/") || is_protected(config, &branch) {
        return Err(PublishError::Denied(format!(
            "source branch {branch:?} is protected or invalid"
        )));
    }
    if !config.remotes.contains_key(remote_name) {
        return Err(PublishError::Denied(format!(
            "unknown publish remote {remote_name:?}"
        )));
    }
    target.branch = branch.clone();
    target.source_branch = Some(branch);
    Ok(())
}

/// Snapshot fallback after a compare-and-swap miss. Refuses when the remote's allowed prefixes do
/// not include `snapshot/`.
pub fn snapshot_fallback(
    config: &PublishConfig,
    remote_name: &str,
    key: &str,
) -> Result<String, PublishError> {
    let remote = config
        .remotes
        .get(remote_name)
        .ok_or_else(|| PublishError::Denied(format!("unknown publish remote {remote_name:?}")))?;
    let branch = branch_from_template(lu_proto::DEFAULT_PUBLISH_BRANCH, key);
    if !prefix_allowed(remote, &branch) || is_protected(config, &branch) {
        return Err(PublishError::State(format!(
            "snapshot fallback branch {branch:?} is not allowed on remote {remote_name:?}"
        )));
    }
    Ok(branch)
}

/// Human-readable effective target for `lu show`. Contains no credential material: only a
/// configured remote name, a target kind and a branch name from the mirror or a validated template.
pub fn target_text(state: &AppState, job: &JobRecord) -> Option<String> {
    let config = state.publish_config()?;
    let spec = job.spec.publish.as_ref()?;
    let key = job
        .spec
        .idempotency_key
        .clone()
        .unwrap_or_else(|| job.id.to_string());
    let mut target = effective_target(
        config,
        spec,
        &job.spec.source.repo,
        &key,
        state.operator_approved(job),
    )
    .ok()?;
    if target.target == PublishTarget::SourceBranch {
        let repo = state.mirror.repo_for_url(&job.spec.source.repo)?;
        let branch = state
            .mirror
            .source_branch(&repo.name, &job.spec.git_ref)
            .ok()?;
        let remote_name = target.remote.clone();
        with_source_branch(config, &remote_name, &mut target, branch).ok()?;
    }
    Some(match target.target {
        PublishTarget::Snapshot => format!("{}:{}", target.remote, target.branch),
        PublishTarget::SourceBranch => {
            format!("{}:{} (source branch)", target.remote, target.branch)
        }
    })
}

fn credentials(token: &str) -> RemoteCallbacks<'_> {
    let token = token.to_owned();
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |_url, username, _cred_type| {
        if token.is_empty() {
            return Cred::default();
        }
        Cred::userpass_plaintext(username.unwrap_or("lu"), &token)
    });
    callbacks
}

enum PushOutcome {
    Pushed,
    CasMiss,
}

/// Pushes one commit to one branch. The refspec is exact and has no `+`, so the remote enforces a
/// fast-forward. `expected` performs the compare-and-swap: the advertised tip is read over the same
/// connection and the branch is not pushed at all when it differs from the job's starting commit.
/// The commit is pushed from the prepared checkout, but the remote itself is anonymous: no
/// tracking ref or remote configuration is written. No tags, deletes, wildcards or other refs are
/// involved.
fn push_one(
    repo_path: &Path,
    url: &str,
    branch: &str,
    commit: Oid,
    expected: Option<Oid>,
    token: &str,
) -> Result<PushOutcome, PublishError> {
    let repo = Repository::open(repo_path)
        .map_err(|e| PublishError::Git(format!("cannot open publish workspace: {e}")))?;
    let mut remote = Repository::remote_anonymous(&repo, url)
        .map_err(|e| PublishError::Git(format!("cannot open publish remote: {e}")))?;
    let mut connection = remote
        .connect_auth(Direction::Push, Some(credentials(token)), None)
        .map_err(|e| PublishError::Git(format!("cannot connect to publish remote: {e}")))?;
    if let Some(expected) = expected {
        let refname = format!("refs/heads/{branch}");
        let advertised = connection
            .list()
            .map_err(|e| PublishError::Git(format!("cannot list publish remote: {e}")))?
            .iter()
            .find(|head| head.name() == refname)
            .map(|head| head.oid());
        if advertised != Some(expected) {
            return Ok(PushOutcome::CasMiss);
        }
    }
    let refname = format!("refs/heads/{branch}");
    let spec = format!("{commit}:{refname}");
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let mut callbacks = credentials(token);
    {
        let statuses = Arc::clone(&statuses);
        callbacks.push_update_reference(move |name, status| {
            statuses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.to_owned(), status.map(str::to_owned)));
            Ok(())
        });
    }
    let mut options = PushOptions::new();
    options.remote_callbacks(callbacks);
    connection
        .remote()
        .push(&[spec.as_str()], Some(&mut options))
        .map_err(|e| PublishError::Git(format!("publish push failed: {e}")))?;
    let updates = statuses
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if updates.len() != 1
        || updates[0].0 != refname
        || updates[0]
            .1
            .as_deref()
            .is_some_and(|status| !status.is_empty())
    {
        return Err(PublishError::State(
            "publish remote rejected the branch update".to_owned(),
        ));
    }
    Ok(PushOutcome::Pushed)
}

struct Prepared {
    dir: tempfile::TempDir,
    commit: Oid,
}

fn prepare_tree(
    mirror_path: &Path,
    base_commit: &str,
    patch: &[u8],
    config: &PublishConfig,
    job: &JobRecord,
) -> Result<Prepared, PublishError> {
    let dir = tempfile::tempdir()
        .map_err(|e| PublishError::Git(format!("cannot prepare git workspace: {e}")))?;
    let repo = Repository::clone(mirror_path.to_string_lossy().as_ref(), dir.path())
        .map_err(|e| PublishError::Git(format!("cannot check out source commit: {e}")))?;
    let oid = Oid::from_str(base_commit)
        .map_err(|_| PublishError::State("job has no valid base commit".to_owned()))?;
    let parent = repo
        .find_commit(oid)
        .map_err(|_| PublishError::State("job base commit is not in the mirror".to_owned()))?;
    repo.checkout_tree(parent.as_object(), Some(CheckoutBuilder::new().force()))
        .map_err(|e| PublishError::Git(format!("cannot check out source commit: {e}")))?;
    repo.set_head_detached(oid)
        .map_err(|e| PublishError::Git(format!("cannot check out source commit: {e}")))?;
    let diff = Diff::from_buffer(patch)
        .map_err(|e| PublishError::State(format!("stored diff is not a git patch: {e}")))?;
    repo.apply(&diff, ApplyLocation::Both, None)
        .map_err(|e| PublishError::State(format!("stored diff does not apply: {e}")))?;
    let mut index = repo
        .index()
        .map_err(|e| PublishError::Git(format!("cannot read git index: {e}")))?;
    let tree_id = index
        .write_tree()
        .map_err(|e| PublishError::Git(format!("cannot write git tree: {e}")))?;
    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| PublishError::Git(format!("cannot read git tree: {e}")))?;
    let time = git2::Time::new(crate::time::now().wall_unix, 0);
    let signature = Signature::new(&config.committer_name, &config.committer_email, &time)
        .map_err(|e| PublishError::Invalid(format!("invalid committer identity: {e}")))?;
    let subject: String = printable(job.spec.task.lines().next().unwrap_or("lu job"))
        .chars()
        .take(72)
        .collect();
    let message = format!("{subject}\n\nLu-Job: {}\n", job.id);
    let commit = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            &message,
            &tree,
            &[&parent],
        )
        .map_err(|e| PublishError::Git(format!("cannot create publish commit: {e}")))?;
    Ok(Prepared { dir, commit })
}

fn read_token(path: &str, remote_name: &str) -> Result<String, PublishError> {
    lu_config::read_private_file(
        &format!("[publish.remotes.{remote_name}] token_file"),
        Path::new(path),
        lu_config::MAX_PUBLISH_TOKEN_BYTES,
    )
    .map(|token| token.trim().to_owned())
    .map_err(|error| PublishError::Invalid(error.to_string()))
}

fn read_diff(state: &AppState, job: &JobRecord) -> Result<Vec<u8>, PublishError> {
    let allow = state.publish_config().is_some_and(|c| c.allow_scrubbed);
    if job.patch_scrubbed && !allow {
        return Err(PublishError::State(
            "the stored diff was changed by scrubbing; enable [publish] allow_scrubbed to publish it"
                .to_owned(),
        ));
    }
    if job.patch_secret_hits.count > 0 && !allow {
        return Err(PublishError::State(
            format!(
                "the stored diff has {} high-confidence patch secret hit(s) (rules: {}); enable [publish] allow_scrubbed to publish it",
                job.patch_secret_hits.count,
                job.patch_secret_hits.rules.join(", ")
            ),
        ));
    }
    let path = state.artifacts_dir.join(job.id.as_str()).join("diff.patch");
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|_| PublishError::State("job has no stored diff to publish".to_owned()))?;
    if !meta.file_type().is_file() || meta.len() > 32 * 1024 * 1024 {
        return Err(PublishError::State(
            "job has no usable stored diff".to_owned(),
        ));
    }
    std::fs::read(&path).map_err(|_| PublishError::State("cannot read stored diff".to_owned()))
}

fn mirror_path_for(
    state: &AppState,
    source_repo: &str,
) -> Result<std::path::PathBuf, PublishError> {
    let repo = state.mirror.repo_for_url(source_repo).ok_or_else(|| {
        PublishError::Denied("source repository is not configured as a mirror".to_owned())
    })?;
    let root = state
        .artifacts_dir
        .parent()
        .ok_or_else(|| PublishError::State("artifacts directory has no parent".to_owned()))?;
    Ok(root.join("mirrors").join(format!("{}.git", repo.name)))
}

/// Runs configured gate commands against the prepared tree. Gate commands are operator-authored
/// argv, never shell text from a payload. A timeout or spawn failure is a `fail` result.
fn run_gates(
    prepared: &Prepared,
    config: &PublishConfig,
    spec: &PublishSpec,
    job: &JobRecord,
) -> Vec<PublishGateRecord> {
    let mut records = Vec::new();
    for name in &spec.gates {
        let Some(gate) = config.gates.get(name) else {
            records.push(PublishGateRecord {
                name: name.clone(),
                job_id: String::new(),
                result: "error".to_owned(),
                exit_code: None,
            });
            break;
        };
        let argv: Vec<&str> = gate.command.split_whitespace().collect();
        let result = if argv.is_empty() {
            Err("empty gate command".to_owned())
        } else {
            run_gate_command(&argv, gate.timeout_secs, prepared.dir.path())
        };
        let (passed, exit_code) = match result {
            Ok(code) => (code == gate.expected_exit_code, Some(code)),
            Err(_) => (false, None),
        };
        records.push(PublishGateRecord {
            name: name.clone(),
            job_id: format!("gate-{}-{}", job.id, name),
            result: if passed { "pass" } else { "fail" }.to_owned(),
            exit_code,
        });
        if !passed {
            break;
        }
    }
    records
}

fn run_gate_command(argv: &[&str], timeout_secs: u64, dir: &Path) -> Result<i32, String> {
    let mut child = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start gate: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().ok_or_else(|| "gate signalled".to_owned()),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("gate timed out".to_owned());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(format!("cannot wait for gate: {e}")),
        }
    }
}

fn base_record(job: &JobRecord, target: &EffectiveTarget, detail: &str) -> PublishRecord {
    PublishRecord {
        detail: detail.to_owned(),
        branch: target.branch.clone(),
        remote: target.remote.clone(),
        base_commit: job.spec.base_commit.clone().unwrap_or_default(),
        commit: None,
        gates: Vec::new(),
        target: target.target,
        source_branch: target.source_branch.clone(),
    }
}

/// Records the process metric and the audit log. The store record itself is written by
/// `save_record`, which owns the final detail code; this helper only reports the outcome.
fn record_outcome(
    state: &AppState,
    job_id: &lu_proto::JobId,
    remote: &str,
    target: PublishTarget,
    result: &str,
) {
    state.record_publish_total(target.as_str(), result);
    state.record_audit(
        lu_audit::Event::Publish,
        Some(job_id.as_str()),
        None,
        "operator",
        lu_audit::Data::new()
            .text(lu_audit::Field::Target, target.as_str())
            .text(lu_audit::Field::Result, result)
            .text(lu_audit::Field::Remote, remote)
            .text(lu_audit::Field::State, result),
    );
}

async fn save_record(state: &Arc<AppState>, job_id: &lu_proto::JobId, record: &PublishRecord) {
    let store = Arc::clone(&state.store);
    let value = record.clone();
    let id = job_id.clone();
    let at = crate::time::now();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = store.record_publish(&id, &value, at);
    })
    .await;
}

async fn finish(
    state: &Arc<AppState>,
    job_id: &lu_proto::JobId,
    mut record: PublishRecord,
    result: &str,
) -> PublishRecord {
    record.detail = result.to_owned();
    save_record(state, job_id, &record).await;
    record_outcome(state, job_id, record.remote.as_str(), record.target, result);
    record
}

/// Performs the whole publish attempt. Precondition failures are returned as typed errors before
/// any git work; runtime failures are recorded as a `failed` publish state.
pub async fn publish(state: Arc<AppState>, job: JobRecord) -> Result<PublishRecord, PublishError> {
    let config = state
        .publish_config()
        .cloned()
        .ok_or(PublishError::NotConfigured)?;
    let spec = job
        .spec
        .publish
        .as_ref()
        .ok_or(PublishError::NoSpec)?
        .clone();
    let key = job
        .spec
        .idempotency_key
        .clone()
        .unwrap_or_else(|| job.id.to_string());
    let mut target = effective_target(
        &config,
        &spec,
        &job.spec.source.repo,
        &key,
        state.operator_approved(&job),
    )
    .inspect_err(|error| {
        if spec.target == PublishTarget::SourceBranch {
            record_outcome(
                &state,
                &job.id,
                "",
                PublishTarget::SourceBranch,
                error.as_str(),
            );
        }
    })?;
    if target.target == PublishTarget::SourceBranch {
        let name = match state.mirror.repo_for_url(&job.spec.source.repo) {
            Some(repo) => repo.name.clone(),
            None => {
                let error = PublishError::Denied(
                    "source repository is not configured as a mirror".to_owned(),
                );
                record_outcome(
                    &state,
                    &job.id,
                    target.remote.as_str(),
                    target.target,
                    error.as_str(),
                );
                return Err(error);
            }
        };
        let git_ref = job.spec.git_ref.clone();
        let mirror = Arc::clone(&state.mirror);
        let branch = match tokio::task::spawn_blocking(move || {
            mirror.source_branch(&name, &git_ref)
        })
        .await
        {
            Ok(Ok(branch)) => branch,
            Ok(Err(error)) => {
                let error = PublishError::Denied(error.to_string());
                record_outcome(
                    &state,
                    &job.id,
                    target.remote.as_str(),
                    target.target,
                    error.as_str(),
                );
                return Err(error);
            }
            Err(_) => {
                let error = PublishError::State("mirror resolution was interrupted".to_owned());
                record_outcome(
                    &state,
                    &job.id,
                    target.remote.as_str(),
                    target.target,
                    error.as_str(),
                );
                return Err(error);
            }
        };
        let remote_name = target.remote.clone();
        if let Err(error) = with_source_branch(&config, &remote_name, &mut target, branch) {
            record_outcome(
                &state,
                &job.id,
                remote_name.as_str(),
                PublishTarget::SourceBranch,
                error.as_str(),
            );
            return Err(error);
        }
    }
    let mut record = base_record(&job, &target, "gates_running");
    save_record(&state, &job.id, &record).await;
    let mirror = match mirror_path_for(&state, &job.spec.source.repo) {
        Ok(path) => path,
        Err(_) => return Ok(finish(&state, &job.id, record, "failed").await),
    };
    let base_commit = match job.spec.base_commit.clone() {
        Some(commit) => commit,
        None => return Ok(finish(&state, &job.id, record, "failed").await),
    };
    let base_oid = match Oid::from_str(&base_commit) {
        Ok(oid) => oid,
        Err(_) => return Ok(finish(&state, &job.id, record, "failed").await),
    };
    let diff = match read_diff(&state, &job) {
        Ok(diff) => diff,
        Err(error) => {
            record.detail = error.as_str().to_owned();
            return Ok(finish(&state, &job.id, record, "failed").await);
        }
    };
    let config_for_git = config.clone();
    let job_for_git = job.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_tree(&mirror, &base_commit, &diff, &config_for_git, &job_for_git)
    })
    .await;
    let prepared = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            record.detail = error.as_str().to_owned();
            return Ok(finish(&state, &job.id, record, "failed").await);
        }
        Err(_) => return Ok(finish(&state, &job.id, record, "failed").await),
    };
    record.gates = run_gates(&prepared, &config, &spec, &job);
    if record.gates.iter().any(|gate| gate.result != "pass") {
        if target.target == PublishTarget::SourceBranch {
            return Ok(finish(&state, &job.id, record, "gates_failed").await);
        }
        if spec.on_gate_failure != lu_proto::GateFailureMode::PushLabelled {
            return Ok(finish(&state, &job.id, record, "hold").await);
        }
        let labelled = format!("{}-failing-gates", target.branch);
        let remote = &config.remotes[&target.remote];
        if !prefix_allowed(remote, &labelled) {
            return Ok(finish(&state, &job.id, record, "hold").await);
        }
        target.branch = labelled;
        record.branch = target.branch.clone();
    }
    let token = match read_token(&config.remotes[&target.remote].token_file, &target.remote) {
        Ok(token) => token,
        Err(error) => {
            record.detail = error.as_str().to_owned();
            return Ok(finish(&state, &job.id, record, "failed").await);
        }
    };
    let url = config.remotes[&target.remote]
        .test_file_url
        .clone()
        .unwrap_or_else(|| config.remotes[&target.remote].url.clone());
    let max_pushes = config.remotes[&target.remote].max_pushes_per_hour;
    let ticket = match state.reserve_publish_push(&target.remote, max_pushes) {
        Ok(ticket) => ticket,
        Err(retry_after) => {
            let error = PublishError::RateLimited(retry_after);
            record_outcome(
                &state,
                &job.id,
                target.remote.as_str(),
                target.target,
                error.as_str(),
            );
            return Err(error);
        }
    };
    if target.target == PublishTarget::SourceBranch {
        let outcome = match push_one(
            prepared.dir.path(),
            &url,
            &target.branch,
            prepared.commit,
            Some(base_oid),
            &token,
        )
        .map_err(|error| error.scrub(&token))
        {
            Ok(outcome) => outcome,
            Err(error) => {
                state.release_publish_push(&target.remote, ticket);
                record.detail = error.as_str().to_owned();
                return Ok(finish(&state, &job.id, record, "failed").await);
            }
        };
        if let PushOutcome::CasMiss = outcome {
            let branch = match snapshot_fallback(&config, &target.remote, &key) {
                Ok(branch) => branch,
                Err(_) => {
                    state.release_publish_push(&target.remote, ticket);
                    return Ok(finish(&state, &job.id, record, "no_snapshot_prefix").await);
                }
            };
            match push_one(
                prepared.dir.path(),
                &url,
                &branch,
                prepared.commit,
                None,
                &token,
            )
            .map_err(|error| error.scrub(&token))
            {
                Ok(PushOutcome::Pushed) => {
                    record.branch = branch;
                    record.commit = Some(prepared.commit.to_string());
                    return Ok(
                        finish(&state, &job.id, record, "published_snapshot_fallback").await,
                    );
                }
                Ok(PushOutcome::CasMiss) => unreachable!("snapshot pushes do not check a lease"),
                Err(error) => {
                    state.release_publish_push(&target.remote, ticket);
                    record.detail = error.as_str().to_owned();
                    return Ok(finish(&state, &job.id, record, "failed").await);
                }
            }
        }
    } else {
        match push_one(
            prepared.dir.path(),
            &url,
            &target.branch,
            prepared.commit,
            None,
            &token,
        )
        .map_err(|error| error.scrub(&token))
        {
            Ok(PushOutcome::Pushed) => {}
            Ok(PushOutcome::CasMiss) => unreachable!("snapshot pushes do not check a lease"),
            Err(error) => {
                state.release_publish_push(&target.remote, ticket);
                record.detail = error.as_str().to_owned();
                return Ok(finish(&state, &job.id, record, "failed").await);
            }
        }
    }
    record.commit = Some(prepared.commit.to_string());
    Ok(finish(&state, &job.id, record, "published").await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use lu_config::{PublishConfig, PublishRemote};

    fn config() -> PublishConfig {
        let mut remote = PublishRemote {
            url: "https://forge.invalid/org/target.git".to_owned(),
            token_file: "/tmp/lu-token".to_owned(),
            branch_prefixes: vec!["snapshot/".to_owned()],
            allow_source_branch_push: false,
            max_pushes_per_hour: 10,
            test_file_url: None,
        };
        let mut config = PublishConfig {
            remotes: BTreeMap::new(),
            gates: BTreeMap::new(),
            committer_name: "Lu Publish".to_owned(),
            committer_email: "lu-publish@localhost".to_owned(),
            allow_scrubbed: false,
            redirects: BTreeMap::new(),
            protected_patterns: Vec::new(),
        };
        config.remotes.insert("forge".to_owned(), remote.clone());
        remote.url = "https://other.invalid/team/repo.git".to_owned();
        remote.allow_source_branch_push = true;
        config.remotes.insert("other".to_owned(), remote);
        config
    }

    fn spec(target: PublishTarget, allow: bool) -> PublishSpec {
        PublishSpec {
            branch: None,
            gates: Vec::new(),
            on_gate_failure: Default::default(),
            remote: None,
            target,
            allow_source_branch: allow,
        }
    }

    #[test]
    fn source_key_normalises_urls() {
        assert_eq!(
            source_key("https://forge.invalid/org/repo.git").as_deref(),
            Some("forge.invalid/org/repo")
        );
        assert_eq!(
            source_key("https://user:pw@forge.invalid:8443/org/repo").as_deref(),
            Some("forge.invalid:8443/org/repo")
        );
        assert_eq!(source_key("not a url"), None);
    }

    #[test]
    fn glob_matches_star_and_literal() {
        assert!(glob_matches("forge/*/repo", "forge/team/repo"));
        assert!(!glob_matches("forge/*/repo", "forge/team/other"));
        assert!(glob_matches("release*", "release/2026.11"));
        assert!(!glob_matches("release*", "hotfix/1"));
    }

    #[test]
    fn redirects_prefer_exact_then_most_literal_pattern() {
        let mut config = config();
        config
            .redirects
            .insert("forge.invalid/*/repo".to_owned(), "forge".to_owned());
        config
            .redirects
            .insert("forge.invalid/team/*".to_owned(), "other".to_owned());
        assert_eq!(
            redirect_for(&config, "forge.invalid/team/repo").unwrap().1,
            "other"
        );
        config
            .redirects
            .insert("forge.invalid/team/repo".to_owned(), "forge".to_owned());
        assert_eq!(
            redirect_for(&config, "forge.invalid/team/repo").unwrap().1,
            "forge"
        );
    }

    #[test]
    fn unmatched_source_remote_is_default_deny() {
        let config = config();
        let error = effective_target(
            &config,
            &spec(PublishTarget::Snapshot, false),
            "https://stranger.invalid/org/repo.git",
            "key",
            true,
        )
        .unwrap_err();
        assert!(matches!(error, PublishError::Denied(_)));
    }

    #[test]
    fn source_that_is_a_publish_remote_needs_no_redirect() {
        let target = effective_target(
            &config(),
            &spec(PublishTarget::Snapshot, false),
            "https://forge.invalid/org/target.git",
            "key",
            true,
        )
        .unwrap();
        assert_eq!(target.remote, "forge");
        assert_eq!(target.branch, "snapshot/key");
    }

    #[test]
    fn redirect_overrides_the_source_remote() {
        let mut config = config();
        config
            .redirects
            .insert("forge.invalid/org/target".to_owned(), "other".to_owned());
        let target = effective_target(
            &config,
            &spec(PublishTarget::Snapshot, false),
            "https://forge.invalid/org/target.git",
            "key",
            true,
        )
        .unwrap();
        assert_eq!(target.remote, "other");
    }

    #[test]
    fn explicit_remote_wins_over_redirect() {
        let mut config = config();
        config
            .redirects
            .insert("forge.invalid/org/target".to_owned(), "other".to_owned());
        let mut publish_spec = spec(PublishTarget::Snapshot, false);
        publish_spec.remote = Some("forge".to_owned());
        let target = effective_target(
            &config,
            &publish_spec,
            "https://forge.invalid/org/target.git",
            "key",
            true,
        )
        .unwrap();
        assert_eq!(target.remote, "forge");
    }

    #[test]
    fn every_source_branch_opt_in_is_required() {
        let source = "https://forge.invalid/org/target.git";
        let mut config = config();
        assert!(matches!(
            effective_target(
                &config,
                &spec(PublishTarget::SourceBranch, true),
                source,
                "k",
                true
            ),
            Err(PublishError::Denied(_))
        ));
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
        assert!(matches!(
            effective_target(
                &config,
                &spec(PublishTarget::SourceBranch, false),
                source,
                "k",
                true
            ),
            Err(PublishError::Denied(_))
        ));
        assert!(matches!(
            effective_target(
                &config,
                &spec(PublishTarget::SourceBranch, true),
                source,
                "k",
                false
            ),
            Err(PublishError::Denied(_))
        ));
        assert!(effective_target(
            &config,
            &spec(PublishTarget::SourceBranch, true),
            source,
            "k",
            true
        )
        .is_ok());
    }

    #[test]
    fn protected_names_are_refused() {
        let config = config();
        for name in ["main", "master", "work", "release/1.0"] {
            assert!(is_protected(&config, name));
        }
        let mut target = EffectiveTarget {
            remote: "other".to_owned(),
            target: PublishTarget::SourceBranch,
            branch: String::new(),
            source_branch: None,
        };
        assert!(with_source_branch(&config, "other", &mut target, "main".to_owned()).is_err());
        assert!(with_source_branch(&config, "other", &mut target, "feature/x".to_owned()).is_ok());
        assert_eq!(target.source_branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn configured_protected_patterns_extend_the_builtin_list() {
        let mut config = config();
        config.protected_patterns.push("stable*".to_owned());
        assert!(is_protected(&config, "stable-1"));
        assert!(!is_protected(&config, "feature"));
    }

    #[test]
    fn snapshot_fallback_requires_an_allowed_prefix() {
        let mut config = config();
        assert_eq!(
            snapshot_fallback(&config, "forge", "key").unwrap(),
            "snapshot/key"
        );
        config.remotes.get_mut("forge").unwrap().branch_prefixes = vec!["review/".to_owned()];
        assert!(matches!(
            snapshot_fallback(&config, "forge", "key"),
            Err(PublishError::State(_))
        ));
    }

    #[test]
    fn credentials_are_scrubbed_from_errors() {
        let error =
            PublishError::Git("fatal: token ghp_secret for host".to_owned()).scrub("ghp_secret");
        assert_eq!(error.message(), "fatal: token [redacted] for host");
    }
}
