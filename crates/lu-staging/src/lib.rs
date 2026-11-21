//! Job staging: a fresh checkout per job, diff export, artifact size caps,
//! safe cleanup of work directories.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

/// Error returned by staging, diff export and cleanup operations.
#[derive(Debug)]
pub enum StageError {
    /// The configured work root is invalid.
    InvalidWorkRoot(String),
    /// A job id did not satisfy the validation rules.
    InvalidJobId(String),
    /// A git ref was refused.
    InvalidRef(String),
    /// A base commit id was refused.
    InvalidCommit(String),
    /// The target directory already exists.
    AlreadyExists(PathBuf),
    /// I/O failure.
    Io(io::Error),
    /// A git command failed.
    Git { message: String },
    /// The produced diff exceeded the configured cap.
    TooLarge,
    /// A generated base pack exceeded its size cap.
    PackTooLarge { cap: usize },
    /// The repository tracks a path under `.lu/`, which must never enter an exported diff.
    TrackedLuPath(String),
    /// An input directory or file was a symlink, or its name was unsafe.
    UnsafeInputPath(String),
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageError::InvalidWorkRoot(msg) => write!(f, "invalid work root: {msg}"),
            StageError::InvalidJobId(msg) => write!(f, "invalid job id: {msg}"),
            StageError::InvalidRef(msg) => write!(f, "invalid git ref: {msg}"),
            StageError::InvalidCommit(msg) => write!(f, "invalid base commit: {msg}"),
            StageError::AlreadyExists(p) => write!(f, "directory already exists: {}", p.display()),
            StageError::Io(e) => write!(f, "io error: {e}"),
            StageError::Git { message } => write!(f, "git error: {message}"),
            StageError::TooLarge => write!(f, "output exceeded size cap"),
            StageError::PackTooLarge { cap } => {
                write!(f, "base pack exceeded the {cap}-byte cap")
            }
            StageError::TrackedLuPath(path) => write!(
                f,
                "repository tracks {path:?} under .lu/; refusing to stage a job whose own paths could enter diff.patch"
            ),
            StageError::UnsafeInputPath(path) => {
                write!(f, "unsafe input path {path:?}: only regular files are allowed")
            }
        }
    }
}

impl std::error::Error for StageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StageError {
    fn from(e: io::Error) -> Self {
        StageError::Io(e)
    }
}

/// Error returned by artifact directory operations.
#[derive(Debug)]
pub enum ArtifactError {
    InvalidRoot(String),
    InvalidJobId(String),
    AlreadyExists(PathBuf),
    Io(io::Error),
    TooLarge,
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtifactError::InvalidRoot(msg) => write!(f, "invalid artifact root: {msg}"),
            ArtifactError::InvalidJobId(msg) => write!(f, "invalid job id: {msg}"),
            ArtifactError::AlreadyExists(p) => {
                write!(f, "directory already exists: {}", p.display())
            }
            ArtifactError::Io(e) => write!(f, "io error: {e}"),
            ArtifactError::TooLarge => write!(f, "output exceeded size cap"),
        }
    }
}

impl std::error::Error for ArtifactError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArtifactError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ArtifactError {
    fn from(e: io::Error) -> Self {
        ArtifactError::Io(e)
    }
}

/// Why a dependency patch did not apply to a staged checkout.
#[derive(Debug)]
pub enum ApplyError {
    /// The patch was malformed or git could not read or write the tree.
    Git(String),
    /// No apply strategy succeeded. `detail` names the failing hunk and file when known.
    Conflict {
        /// Paths left in conflict by a three-way merge, or the patch's target file.
        paths: Vec<String>,
        /// Extra context, including the failing hunk and file, when available.
        detail: Option<String>,
    },
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Git(message) => write!(f, "apply error: {message}"),
            ApplyError::Conflict { paths, detail } => {
                write!(f, "apply conflict in {}", paths.join(", "))?;
                if let Some(detail) = detail {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ApplyError {}

/// What an agent does when a collected input does not apply cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Stop before the harness and report the conflict.
    Fail,
    /// Leave labelled conflict markers in the worktree and keep applying later inputs.
    Markers,
    /// Leave this input unapplied and keep applying later inputs.
    Skip,
}

/// How one collected input was applied to a staged checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputApplyResult {
    /// Whether any part of the input's patch reached the worktree. Marker conflicts count.
    pub applied: bool,
    /// Files carrying conflict markers, in merge order.
    pub conflicted_paths: Vec<String>,
    /// First failing file and hunk, or another short reason, when the patch was not clean.
    pub detail: Option<String>,
}

/// A validated job identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobId(String);

impl JobId {
    /// Validate and construct a job id.
    ///
    /// Rules: 1 to 64 characters of `A-Za-z0-9._-`, not starting with `.`.
    pub fn new(id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        validate_job_id(&id)?;
        Ok(JobId(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for JobId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn validate_job_id(id: &str) -> Result<(), String> {
    let len = id.len();
    if !(1..=64).contains(&len) {
        return Err("job id length must be 1 to 64 characters".to_string());
    }
    if id.starts_with('.') {
        return Err("job id must not start with '.'".to_string());
    }
    for c in id.chars() {
        if !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
            return Err(format!("job id contains invalid character '{c}'"));
        }
    }
    Ok(())
}

/// The tree-local directory where collected dependency artifacts live.
pub const INPUTS_DIR: &str = ".lu/inputs";

/// Whether a diff path is inside `.lu/`, the tree-local collection directory.
pub fn is_lu_path(path: &Path) -> bool {
    path.components().find_map(|part| match part {
        Component::Normal(name) => Some(name),
        Component::CurDir => None,
        _ => Some(OsStr::new("")),
    }) == Some(OsStr::new(".lu"))
}

/// Picks a safe directory name for one collected input. A hostile or empty idempotency key falls
/// back to the job id, which the controller has already validated.
pub fn safe_input_name(key: Option<&str>, job_id: &str) -> String {
    if let Some(name) = key {
        let safe = !name.is_empty()
            && name.len() <= MAX_INPUT_NAME_BYTES
            && !name.starts_with('.')
            && name != ".."
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b));
        if safe {
            return name.to_owned();
        }
    }
    job_id.to_owned()
}

const MAX_INPUT_NAME_BYTES: usize = 128;

/// Refuses a checkout that tracks anything under `.lu/`: such a path could leak into `diff.patch`
/// even when collection is off, because it is part of the job's own tree.
pub fn reject_tracked_lu(staged: &StagedDir) -> Result<(), StageError> {
    let repo = git2::Repository::open(staged.path()).map_err(git_err)?;
    let index = repo.index().map_err(git_err)?;
    for entry in index.iter() {
        let path = String::from_utf8_lossy(&entry.path);
        if is_lu_path(Path::new(path.as_ref())) {
            return Err(StageError::TrackedLuPath(path.into_owned()));
        }
    }
    Ok(())
}

/// Creates `.lu/inputs` with restrictive modes, records `.lu/` in the tree-local git exclude file
/// (never the repository's `.gitignore`), and refuses symlinks in the path.
pub fn prepare_inputs(staged: &StagedDir) -> Result<PathBuf, StageError> {
    reject_tracked_lu(staged)?;
    let lu = staged.path().join(".lu");
    refuse_symlink(&lu)?;
    let inputs = lu.join("inputs");
    refuse_symlink(&inputs)?;
    create_private_dir(&lu)?;
    create_private_dir(&inputs)?;
    write_local_exclude(staged)?;
    Ok(inputs)
}

/// Creates one per-input directory under `.lu/inputs`, keeping names unique and refusing links.
pub fn create_input_dir(inputs: &Path, name: &str) -> Result<PathBuf, StageError> {
    if name.is_empty()
        || name.len() > MAX_INPUT_NAME_BYTES
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b))
    {
        return Err(StageError::UnsafeInputPath(name.to_owned()));
    }
    let dir = inputs.join(name);
    refuse_symlink(&dir)?;
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Writes one collected artifact with mode `0600`, refusing to follow or replace a symlink.
pub fn write_input_file(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<(), StageError> {
    const ALLOWED: [&str; 5] = [
        "diff.patch",
        "report.json",
        "run.log",
        "meta.json",
        "INDEX.md",
    ];
    if !ALLOWED.contains(&file_name) {
        return Err(StageError::UnsafeInputPath(file_name.to_owned()));
    }
    let path = dir.join(file_name);
    refuse_symlink(&path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(StageError::Io)?;
    file.write_all(bytes).map_err(StageError::Io)?;
    file.flush().map_err(StageError::Io)
}

fn create_private_dir(path: &Path) -> Result<(), StageError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(StageError::UnsafeInputPath(path.display().to_string()));
        }
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(StageError::UnsafeInputPath(path.display().to_string()));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(StageError::Io(e)),
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).map_err(StageError::Io)
}

fn refuse_symlink(path: &Path) -> Result<(), StageError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(StageError::UnsafeInputPath(path.display().to_string()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StageError::Io(e)),
    }
}

fn write_local_exclude(staged: &StagedDir) -> Result<(), StageError> {
    let git_dir = staged.path().join(".git");
    let info = git_dir.join("info");
    create_private_dir(&info)?;
    let exclude = info.join("exclude");
    refuse_symlink(&exclude)?;
    let existing = match fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(StageError::Io(e)),
    };
    if existing.lines().any(|line| line.trim() == ".lu/") {
        return Ok(());
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(exclude).map_err(StageError::Io)?;
    file.write_all(b"\n.lu/\n").map_err(StageError::Io)
}

/// Where to obtain the repository for a staged job.
#[derive(Debug, Clone)]
pub enum Source {
    /// `https://`, `ssh://`, `file://` URLs or scp-style `host:path`.
    Url(String),
    /// An absolute path to a local repository.
    Path(PathBuf),
}

/// Request describing how to create a staged checkout.
#[derive(Debug, Clone)]
pub struct StageRequest {
    pub job_id: String,
    pub source: Source,
    pub git_ref: String,
}

/// A successfully staged job checkout.
#[derive(Debug, Clone)]
pub struct StagedDir {
    path: PathBuf,
    job_id: String,
}

impl StagedDir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }
}

/// Largest base pack the controller will generate and the agent will accept.
pub const PACK_CAP_BYTES: usize = 256 * 1024 * 1024;

fn git_err(error: git2::Error) -> StageError {
    StageError::Git {
        message: error.to_string(),
    }
}

/// Packs one commit, its tree, and every reachable blob or subtree, but not its parents.
pub fn pack_commit(repo: &git2::Repository, commit: git2::Oid) -> Result<Vec<u8>, StageError> {
    pack_commit_capped(repo, commit, PACK_CAP_BYTES)
}

/// [`pack_commit`] with an explicit cap, mostly so the cap can be tested cheaply.
pub fn pack_commit_capped(
    repo: &git2::Repository,
    commit: git2::Oid,
    cap: usize,
) -> Result<Vec<u8>, StageError> {
    let commit = repo.find_commit(commit).map_err(git_err)?;
    let tree = commit.tree().map_err(git_err)?;
    let mut builder = repo.packbuilder().map_err(git_err)?;
    builder.insert_object(commit.id(), None).map_err(git_err)?;
    builder.insert_tree(tree.id()).map_err(git_err)?;
    let mut buf = git2::Buf::new();
    builder.write_buf(&mut buf).map_err(git_err)?;
    if buf.len() > cap {
        return Err(StageError::PackTooLarge { cap });
    }
    Ok(buf.to_vec())
}

/// Manages per-job staging directories under a fixed work root.
/// The file inside `.git` that says this crate staged the directory.
const STAGED_MARKER: &str = "lu-staged";

#[derive(Debug, Clone)]
pub struct Staging {
    work_root: PathBuf,
}

/// A patch and the paths it changes, including deletions and renames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedDiff {
    pub bytes: Vec<u8>,
    pub changed_paths: Vec<String>,
}

impl Staging {
    /// Create a staging manager rooted at `work_root`.
    ///
    /// The root must exist, be an absolute path, not be `/` or a bare
    /// top-level directory, and not be a symlink.
    pub fn new(work_root: impl AsRef<Path>) -> Result<Self, StageError> {
        let work_root = work_root.as_ref();
        validate_work_root(work_root)?;
        // Keep the provided absolute path form after validation.
        Ok(Staging {
            work_root: work_root.to_path_buf(),
        })
    }

    pub fn work_root(&self) -> &Path {
        &self.work_root
    }

    /// Create a fresh checkout for the given request.
    pub fn stage(&self, request: StageRequest) -> Result<StagedDir, StageError> {
        validate_job_id(&request.job_id).map_err(StageError::InvalidJobId)?;
        validate_git_ref(&request.git_ref).map_err(StageError::InvalidRef)?;

        let target = self.work_root.join(&request.job_id);
        // Never reuse an existing directory.
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err(StageError::AlreadyExists(target)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(StageError::Io(e)),
        }

        let result = self.stage_inner(&request, &target);
        if let Err(e) = result.and_then(|()| {
            reject_tracked_lu(&StagedDir {
                path: target.clone(),
                job_id: request.job_id.clone(),
            })
        }) {
            // A failed stage removes what it created.
            let _ = cleanup_created_path(&target);
            return Err(e);
        }

        set_dir_mode_0700(&target)?;
        Ok(StagedDir {
            path: target,
            job_id: request.job_id,
        })
    }

    /// Stages a checkout from a pack made by [`pack_commit`], without invoking `git`.
    ///
    /// The pack is indexed into an empty repository, the commit is made the root of a local
    /// branch via `.git/shallow`, and the tree is checked out. The resulting directory is
    /// indistinguishable from a clone to [`Staging::export_diff`] and [`Staging::share_with_group`].
    pub fn stage_from_pack(
        &self,
        request: StageRequest,
        commit_id: &str,
        pack: &[u8],
    ) -> Result<StagedDir, StageError> {
        validate_job_id(&request.job_id).map_err(StageError::InvalidJobId)?;
        validate_git_ref(&request.git_ref).map_err(StageError::InvalidRef)?;
        let oid = validate_commit_id(commit_id)?;
        if pack.len() > PACK_CAP_BYTES {
            return Err(StageError::PackTooLarge {
                cap: PACK_CAP_BYTES,
            });
        }

        let target = self.work_root.join(&request.job_id);
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err(StageError::AlreadyExists(target)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(StageError::Io(e)),
        }

        let result = self.stage_from_pack_inner(&target, oid, pack);
        if let Err(e) = result.and_then(|()| {
            reject_tracked_lu(&StagedDir {
                path: target.clone(),
                job_id: request.job_id.clone(),
            })
        }) {
            let _ = cleanup_created_path(&target);
            return Err(e);
        }
        set_dir_mode_0700(&target)?;
        Ok(StagedDir {
            path: target,
            job_id: request.job_id,
        })
    }

    fn stage_from_pack_inner(
        &self,
        target: &Path,
        oid: git2::Oid,
        pack: &[u8],
    ) -> Result<(), StageError> {
        fs::create_dir(target)?;
        set_dir_mode_0700(target)?;
        let repo = git2::Repository::init(target).map_err(git_err)?;
        let git_dir = repo.path().to_path_buf();
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir)?;

        let mut indexer = git2::Indexer::new(None, &pack_dir, 0o600, false).map_err(git_err)?;
        indexer.write_all(pack)?;
        indexer.commit().map_err(git_err)?;

        let commit = repo.find_commit(oid).map_err(git_err)?;
        // A shallow root lets the checkout and diff treat this commit as complete even though
        // the parent that the commit object names was deliberately left out of the pack.
        fs::write(git_dir.join("shallow"), format!("{}\n", oid))?;
        repo.reference("refs/heads/lu-base", oid, true, "base pack")
            .map_err(git_err)?;
        repo.set_head("refs/heads/lu-base").map_err(git_err)?;
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        repo.checkout_tree(commit.as_object(), Some(&mut checkout))
            .map_err(git_err)?;
        fs::write(git_dir.join(STAGED_MARKER), b"lu-staging\n")?;
        Ok(())
    }

    fn stage_inner(&self, request: &StageRequest, target: &Path) -> Result<(), StageError> {
        fs::create_dir(target)?;
        // Ensure mode is restrictive early as well.
        set_dir_mode_0700(target)?;

        // Clone without checking out a specific ref implicitly beyond default,
        // then fetch/checkout the requested ref explicitly.
        match &request.source {
            Source::Url(url) => {
                validate_url_source(url)?;
                run_git(
                    None,
                    [
                        "clone",
                        "--no-checkout",
                        "--",
                        url.as_str(),
                        path_str(target)?,
                    ],
                )?;
            }
            Source::Path(path) => {
                if !path.is_absolute() {
                    return Err(StageError::Git {
                        message: "local source path must be absolute".to_string(),
                    });
                }
                run_git(
                    None,
                    [
                        "clone",
                        "--no-checkout",
                        "--no-local",
                        "--",
                        path_str(path)?,
                        path_str(target)?,
                    ],
                )?;
            }
        }

        // Checkout the requested ref (branch, tag or commit).
        run_git(
            Some(target),
            ["checkout", "--force", request.git_ref.as_str(), "--"],
        )?;
        // Marks the directory as one this crate made, so a bulk clean never removes anything
        // else. Inside `.git`, so it never shows up in a diff.
        fs::write(target.join(".git").join(STAGED_MARKER), b"lu-staging\n")?;
        Ok(())
    }

    /// Export everything the job changed relative to the checked-out commit,
    /// including new untracked files that are not ignored, as a binary-safe patch.
    pub fn export_diff(staged: &StagedDir, cap_bytes: usize) -> Result<Vec<u8>, StageError> {
        Self::export_diff_with_paths(staged, cap_bytes).map(|exported| exported.bytes)
    }

    /// Exports the patch and the changed paths it contains. Paths come from the same diff, with
    /// the same `.lu/` filtering, so expectations see exactly what the job uploads.
    pub fn export_diff_with_paths(
        staged: &StagedDir,
        cap_bytes: usize,
    ) -> Result<ExportedDiff, StageError> {
        let repo = git2::Repository::open(staged.path()).map_err(|e| StageError::Git {
            message: format!("failed to open repository: {e}"),
        })?;
        let head = repo
            .head()
            .and_then(|h| h.peel_to_tree())
            .map_err(|e| StageError::Git {
                message: format!("failed to resolve HEAD tree: {e}"),
            })?;
        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true)
            .show_binary(true)
            .ignore_submodules(true);
        let diff = repo
            .diff_tree_to_workdir_with_index(Some(&head), Some(&mut opts))
            .map_err(|e| StageError::Git {
                message: format!("failed to compute diff: {e}"),
            })?;

        let mut changed_paths = Vec::new();
        for delta in diff.deltas() {
            for file in [delta.old_file(), delta.new_file()] {
                let Some(path) = file.path() else { continue };
                if is_lu_path(path) {
                    continue;
                }
                let text = path.to_string_lossy().into_owned();
                if !changed_paths.contains(&text) {
                    changed_paths.push(text);
                }
            }
        }

        let mut out: Vec<u8> = Vec::new();
        let mut too_large = false;
        let print_res = diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            // `.lu/inputs` is collection scratch space, never job output. Filter here as well
            // as in `.git/info/exclude` so a forced add or an older agent cannot leak it.
            let path = delta.new_file().path().or_else(|| delta.old_file().path());
            if path.is_some_and(is_lu_path) {
                return true;
            }
            let content = line.content();
            let prefix = match line.origin() {
                '+' | '-' | ' ' => Some(line.origin() as u8),
                'F' | 'H' | 'B' => None,
                _ => None,
            };
            let need = content.len() + usize::from(prefix.is_some());
            if out.len().saturating_add(need) > cap_bytes {
                too_large = true;
                return false;
            }
            if let Some(p) = prefix {
                out.push(p);
            }
            out.extend_from_slice(content);
            true
        });
        if too_large {
            return Err(StageError::TooLarge);
        }
        print_res.map_err(|e| StageError::Git {
            message: format!("failed to format diff: {e}"),
        })?;
        Ok(ExportedDiff {
            bytes: out,
            changed_paths,
        })
    }

    /// Apply a dependency's `diff.patch` to a staged checkout, with the default `fail` policy.
    pub fn apply_dependency_diff(staged: &StagedDir, patch: &[u8]) -> Result<(), ApplyError> {
        Self::apply_input_diff(staged, patch, "input", ConflictPolicy::Fail).map(|_| ())
    }

    /// Apply one collected input's diff under `policy`. `name` labels conflict markers.
    ///
    /// Every strategy is tried in order: libgit2 apply, libgit2 three-way, then the git binary's
    /// fuzzy three-way apply. A clean apply contributes its whole patch; a conflict under
    /// `markers` contributes both sides in the worktree, so later inputs still apply on top.
    pub fn apply_input_diff(
        staged: &StagedDir,
        patch: &[u8],
        name: &str,
        policy: ConflictPolicy,
    ) -> Result<InputApplyResult, ApplyError> {
        let label = conflict_label(name);
        let repo = git2::Repository::open(staged.path())
            .map_err(|e| ApplyError::Git(format!("failed to open repository: {e}")))?;
        let diff = match git2::Diff::from_buffer(patch) {
            Ok(diff) => diff,
            Err(error) => {
                let detail = format!("invalid diff: {error}; {}", hunk_detail(patch));
                return match policy {
                    ConflictPolicy::Fail => Err(ApplyError::Git(format!("invalid diff: {error}"))),
                    _ => Ok(unapplied(policy, Vec::new(), detail)),
                };
            }
        };
        let hunk = hunk_detail(patch);

        let mut check = git2::ApplyOptions::new();
        check.check(true);
        if repo
            .apply(&diff, git2::ApplyLocation::WorkDir, Some(&mut check))
            .is_ok()
        {
            // Keep the pre-patch tree so a write failure cannot leave half a patch behind.
            let before = worktree_tree(&repo)?;
            if let Err(e) = repo.apply(&diff, git2::ApplyLocation::WorkDir, None) {
                restore_tree(&repo, &before)?;
                reset_index_to_head(&repo)?;
                return match policy {
                    ConflictPolicy::Fail => {
                        Err(ApplyError::Git(format!("failed to apply diff: {e}")))
                    }
                    _ => Ok(unapplied(
                        policy,
                        Vec::new(),
                        format!("failed to apply diff: {e}; {hunk}"),
                    )),
                };
            }
            return Ok(InputApplyResult {
                applied: true,
                conflicted_paths: Vec::new(),
                detail: None,
            });
        }

        let base = head_tree(&repo)?;
        let ours = worktree_tree(&repo)?;
        let mut theirs_index = match repo.apply_to_tree(&base, &diff, None) {
            Ok(index) => index,
            Err(e) => {
                // libgit2 needs an exact hunk offset here. Try a plain apply, then the git
                // binary's fuzzy three-way apply before giving up on this input.
                let plain = repo.apply(&diff, git2::ApplyLocation::WorkDir, None);
                if plain.is_ok() {
                    reset_index_to_head(&repo)?;
                    return Ok(InputApplyResult {
                        applied: true,
                        conflicted_paths: Vec::new(),
                        detail: None,
                    });
                }
                let plain_error = plain.err().map(|e| e.to_string()).unwrap_or_default();
                match git_apply_3way(staged.path(), patch) {
                    Ok(()) => {
                        reset_index_to_head(&repo)?;
                        return Ok(InputApplyResult {
                            applied: true,
                            conflicted_paths: Vec::new(),
                            detail: None,
                        });
                    }
                    Err(git_error) => {
                        let paths = index_conflict_paths(&repo)?;
                        if !paths.is_empty() && policy == ConflictPolicy::Markers {
                            relabel_git_conflicts(&repo, &paths, &label)?;
                            reset_index_to_head(&repo)?;
                            return Ok(InputApplyResult {
                                applied: true,
                                conflicted_paths: paths,
                                detail: Some(hunk),
                            });
                        }
                        restore_tree(&repo, &ours)?;
                        reset_index_to_head(&repo)?;
                        let detail = format!(
                            "failed to prepare three-way merge: {hunk}: {e}; plain apply: {plain_error}; git apply --3way: {git_error}"
                        );
                        return match policy {
                            ConflictPolicy::Fail => Err(ApplyError::Conflict {
                                paths: patch_paths(patch),
                                detail: Some(detail),
                            }),
                            _ => Ok(unapplied(policy, Vec::new(), detail)),
                        };
                    }
                }
            }
        };
        let theirs_oid = theirs_index
            .write_tree_to(&repo)
            .map_err(|e| ApplyError::Git(format!("failed to write merge tree: {e}")))?;
        let theirs = repo
            .find_tree(theirs_oid)
            .map_err(|e| ApplyError::Git(format!("failed to read merge tree: {e}")))?;
        let mut merged = repo
            .merge_trees(&base, &ours, &theirs, None)
            .map_err(|e| ApplyError::Git(format!("failed to merge trees: {e}")))?;

        if policy == ConflictPolicy::Skip {
            let paths = merge_conflict_paths(&merged)?;
            if !paths.is_empty() {
                return Ok(unapplied(policy, paths, hunk));
            }
        } else {
            let paths = merge_conflict_paths(&merged)?;
            if !paths.is_empty() {
                return match policy {
                    ConflictPolicy::Fail => Err(ApplyError::Conflict {
                        paths,
                        detail: Some(hunk),
                    }),
                    ConflictPolicy::Markers => {
                        let paths = write_merge_conflict_markers(&repo, &merged, &label)?;
                        reset_index_to_head(&repo)?;
                        Ok(InputApplyResult {
                            applied: true,
                            conflicted_paths: paths,
                            detail: Some(hunk),
                        })
                    }
                    ConflictPolicy::Skip => unreachable!("handled above"),
                };
            }
        }

        let merged_oid = merged
            .write_tree_to(&repo)
            .map_err(|e| ApplyError::Git(format!("failed to write merged tree: {e}")))?;
        let merged = repo
            .find_tree(merged_oid)
            .map_err(|e| ApplyError::Git(format!("failed to read merged tree: {e}")))?;
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force().remove_untracked(true);
        repo.checkout_tree(merged.as_object(), Some(&mut checkout))
            .map_err(|e| ApplyError::Git(format!("failed to check out merge: {e}")))?;
        reset_index_to_head(&repo)?;
        Ok(InputApplyResult {
            applied: true,
            conflicted_paths: Vec::new(),
            detail: None,
        })
    }
}

fn hunk_detail(patch: &[u8]) -> String {
    match patch_location(patch) {
        Some((file, line)) if file != "unknown file" => {
            format!("hunk at line {line} in {file}")
        }
        _ => "hunk location unknown".to_owned(),
    }
}

fn patch_paths(patch: &[u8]) -> Vec<String> {
    match patch_location(patch) {
        Some((file, _)) if file != "unknown file" => vec![file],
        _ => Vec::new(),
    }
}

fn conflict_label(name: &str) -> String {
    let mut label: String = name
        .bytes()
        .take(64)
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"._-:".contains(&byte) {
                byte as char
            } else {
                '_'
            }
        })
        .collect();
    if label.is_empty() {
        label.push_str("input");
    }
    label
}

fn unapplied(
    policy: ConflictPolicy,
    conflicted_paths: Vec<String>,
    detail: String,
) -> InputApplyResult {
    debug_assert!(policy != ConflictPolicy::Fail);
    InputApplyResult {
        applied: false,
        conflicted_paths,
        detail: Some(detail),
    }
}

fn reset_index_to_head(repo: &git2::Repository) -> Result<(), ApplyError> {
    let head = head_tree(repo)?;
    let mut index = repo
        .index()
        .map_err(|e| ApplyError::Git(format!("failed to open index: {e}")))?;
    index
        .read_tree(&head)
        .map_err(|e| ApplyError::Git(format!("failed to reset index: {e}")))?;
    index
        .write()
        .map_err(|e| ApplyError::Git(format!("failed to write index: {e}")))
}

fn merge_conflict_paths(merged: &git2::Index) -> Result<Vec<String>, ApplyError> {
    let mut paths = Vec::new();
    let conflicts = merged
        .conflicts()
        .map_err(|e| ApplyError::Git(format!("failed to inspect merge: {e}")))?;
    for conflict in conflicts {
        let conflict =
            conflict.map_err(|e| ApplyError::Git(format!("failed to inspect merge: {e}")))?;
        if let Some(entry) = conflict.our.or(conflict.their).or(conflict.ancestor) {
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

fn index_conflict_paths(repo: &git2::Repository) -> Result<Vec<String>, ApplyError> {
    let index = repo
        .index()
        .map_err(|e| ApplyError::Git(format!("failed to open index: {e}")))?;
    merge_conflict_paths(&index)
}

fn write_merge_conflict_markers(
    repo: &git2::Repository,
    merged: &git2::Index,
    label: &str,
) -> Result<Vec<String>, ApplyError> {
    let mut paths = Vec::new();
    let conflicts = merged
        .conflicts()
        .map_err(|e| ApplyError::Git(format!("failed to inspect merge: {e}")))?;
    for conflict in conflicts {
        let conflict =
            conflict.map_err(|e| ApplyError::Git(format!("failed to inspect merge: {e}")))?;
        let our = conflict.our.as_ref().map(|entry| entry.id);
        let their = conflict.their.as_ref().map(|entry| entry.id);
        let entry = conflict
            .our
            .as_ref()
            .or(conflict.their.as_ref())
            .or(conflict.ancestor.as_ref())
            .ok_or_else(|| ApplyError::Git("merge conflict has no file entry".to_owned()))?;
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        let ours = blob_bytes(repo, our)?;
        let theirs = blob_bytes(repo, their)?;
        write_marker_file(repo, &path, &ours, &theirs, label)?;
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn blob_bytes(repo: &git2::Repository, id: Option<git2::Oid>) -> Result<Vec<u8>, ApplyError> {
    match id {
        Some(id) => Ok(repo
            .find_blob(id)
            .map_err(|e| ApplyError::Git(format!("failed to read merge blob: {e}")))?
            .content()
            .to_vec()),
        None => Ok(Vec::new()),
    }
}

fn safe_worktree_path(repo: &git2::Repository, path: &str) -> Result<PathBuf, ApplyError> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
        || is_lu_path(relative)
    {
        return Err(ApplyError::Git(format!("unsafe conflict path {path:?}")));
    }
    let full = repo
        .workdir()
        .ok_or_else(|| ApplyError::Git("staged checkout has no worktree".to_owned()))?
        .join(relative);
    refuse_symlink(&full).map_err(|e| ApplyError::Git(format!("unsafe conflict path: {e}")))?;
    Ok(full)
}

fn write_marker_file(
    repo: &git2::Repository,
    path: &str,
    ours: &[u8],
    theirs: &[u8],
    label: &str,
) -> Result<(), ApplyError> {
    let full = safe_worktree_path(repo, path)?;
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ApplyError::Git(format!("failed to create conflict directory: {e}")))?;
    }
    let mut out = Vec::with_capacity(ours.len() + theirs.len() + 64);
    out.extend_from_slice(b"<<<<<<< HEAD\n");
    out.extend_from_slice(ours);
    if !ours.ends_with(b"\n") && !ours.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(theirs);
    if !theirs.ends_with(b"\n") && !theirs.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(format!(">>>>>>> {label}\n").as_bytes());
    fs::write(&full, out)
        .map_err(|e| ApplyError::Git(format!("failed to write conflict markers: {e}")))
}

fn relabel_git_conflicts(
    repo: &git2::Repository,
    paths: &[String],
    label: &str,
) -> Result<(), ApplyError> {
    for path in paths {
        let full = safe_worktree_path(repo, path)?;
        let text = match fs::read_to_string(&full) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(ApplyError::Git(format!(
                    "failed to read conflict file {path:?}: {e}"
                )))
            }
        };
        let mut out = String::with_capacity(text.len() + 32);
        for line in text.split_inclusive('\n') {
            if line.trim_end_matches(['\r', '\n']).starts_with(">>>>>>> ") {
                out.push_str(">>>>>>> ");
                out.push_str(label);
                out.push('\n');
            } else {
                out.push_str(line);
            }
        }
        fs::write(&full, out)
            .map_err(|e| ApplyError::Git(format!("failed to relabel conflict file: {e}")))?;
    }
    Ok(())
}

impl Staging {
    /// Remove `<work_root>/<job_id>` safely.
    /// A missing directory is not an error.
    /// Lets `gid` write the working tree of a staged checkout and never the `.git` directory.
    ///
    /// For a harness that runs as another account and cannot run git itself: the agent clones
    /// and diffs, the group edits files. The checkout and every directory in it get the group,
    /// group read and write, and the setgid bit so files made later inherit the group. Symlinks
    /// are re-grouped but never followed. `.git` is left exactly as git made it, so the group
    /// cannot plant a hook or change a ref.
    pub fn share_with_group(&self, staged: &StagedDir, gid: u32) -> Result<(), StageError> {
        use std::os::unix::fs::{lchown, MetadataExt, PermissionsExt};

        fn share(path: &Path, gid: u32) -> io::Result<()> {
            let meta = fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink() {
                return lchown(path, None, Some(gid));
            }
            std::os::unix::fs::chown(path, None, Some(gid))?;
            let mode = meta.mode() & 0o7777;
            if meta.is_dir() {
                fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o2070))?;
                for entry in fs::read_dir(path)? {
                    share(&entry?.path(), gid)?;
                }
            } else {
                let group_exec = if mode & 0o100 != 0 { 0o010 } else { 0 };
                fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o060 | group_exec))?;
            }
            Ok(())
        }

        let root = staged.path();
        std::os::unix::fs::chown(root, None, Some(gid))?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o2770))?;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            share(&entry.path(), gid)?;
        }
        Ok(())
    }

    pub fn clean(&self, job_id: &str) -> Result<(), StageError> {
        validate_job_id(job_id).map_err(StageError::InvalidJobId)?;
        let target = self.work_root.join(job_id);
        self.clean_path_if_safe_job_dir(&target, true)
    }

    /// Remove every other direct child that looks like a job directory and
    /// leave anything else alone.
    pub fn clean_all_except(&self, keep: &[JobId]) -> Result<(), StageError> {
        let entries = match fs::read_dir(&self.work_root) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(StageError::Io(e)),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            // Only touch entries that look like job directories.
            if JobId::new(name_str).is_err() {
                continue;
            }
            if keep.iter().any(|k| k.as_str() == name_str) {
                continue;
            }
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StageError::Io(e)),
            };
            if meta.file_type().is_symlink() || !meta.is_dir() {
                continue;
            }
            // Only directories this crate staged, whatever their names look like.
            if !path.join(".git").join(STAGED_MARKER).is_file() {
                continue;
            }
            self.clean_path_if_safe_job_dir(&path, true)?;
        }
        Ok(())
    }

    fn clean_path_if_safe_job_dir(
        &self,
        target: &Path,
        missing_ok: bool,
    ) -> Result<(), StageError> {
        let meta = match fs::symlink_metadata(target) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound && missing_ok => return Ok(()),
            Err(e) => return Err(StageError::Io(e)),
        };
        if meta.file_type().is_symlink() {
            return Err(StageError::Git {
                message: "refusing to remove symlinked job directory".to_string(),
            });
        }
        if !meta.is_dir() {
            return Err(StageError::Git {
                message: "job path is not a directory".to_string(),
            });
        }
        // Ensure target is a direct child of the canonical work root.
        let canon_root = fs::canonicalize(&self.work_root)?;
        let canon_target = fs::canonicalize(target)?;
        let parent = canon_target.parent().ok_or_else(|| StageError::Git {
            message: "job directory has no parent".to_string(),
        })?;
        if parent != canon_root {
            return Err(StageError::Git {
                message: "job directory is not a direct child of the work root".to_string(),
            });
        }
        // One more component check: direct child means single trailing component.
        let name = canon_target
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| StageError::Git {
                message: "job directory name is not valid utf-8".to_string(),
            })?;
        if JobId::new(name).is_err() {
            return Err(StageError::InvalidJobId(name.to_string()));
        }

        remove_dir_all_robust(&canon_target)?;
        Ok(())
    }
}

/// Best-effort location of the first hunk in a patch, for failure reporting.
fn patch_location(patch: &[u8]) -> Option<(String, usize)> {
    let text = String::from_utf8_lossy(patch);
    let mut file = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.split_whitespace().next().unwrap_or(path);
            let path = path.strip_prefix("b/").unwrap_or(path);
            if path != "/dev/null" {
                file = Some(path.to_owned());
            }
        } else if let Some(header) = line.strip_prefix("@@ ") {
            if let Some(new) = header.split('+').nth(1) {
                let digits: String = new.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(start) = digits.parse::<usize>() {
                    return Some((file.unwrap_or_else(|| "unknown file".to_owned()), start));
                }
            }
        }
    }
    None
}

/// Applies a patch with the git binary's fuzzy three-way apply. A missing git binary is reported
/// as an ordinary error; the caller turns it into a conflict after exhausting all strategies.
fn git_apply_3way(dir: &Path, patch: &[u8]) -> Result<(), String> {
    let mut child = Command::new("git")
        .args(["apply", "--3way", "-"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run git apply --3way: {e}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(patch)
            .map_err(|e| format!("could not feed patch to git apply --3way: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("could not wait for git apply --3way: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if message.is_empty() {
            format!("git apply --3way exited with {}", output.status)
        } else {
            message
        })
    }
}

fn head_tree(repo: &git2::Repository) -> Result<git2::Tree<'_>, ApplyError> {
    repo.head()
        .and_then(|head| head.peel_to_tree())
        .map_err(|e| ApplyError::Git(format!("failed to resolve HEAD tree: {e}")))
}

/// The checkout's current state as a tree, built without touching the real index.
fn worktree_tree(repo: &git2::Repository) -> Result<git2::Tree<'_>, ApplyError> {
    let base = head_tree(repo)?;
    let mut opts = git2::DiffOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .show_binary(true)
        .ignore_submodules(true);
    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&base), Some(&mut opts))
        .map_err(|e| ApplyError::Git(format!("failed to compute working tree: {e}")))?;
    let mut index = repo
        .apply_to_tree(&base, &diff, None)
        .map_err(|e| ApplyError::Git(format!("failed to capture working tree: {e}")))?;
    let oid = index
        .write_tree_to(repo)
        .map_err(|e| ApplyError::Git(format!("failed to write working tree: {e}")))?;
    repo.find_tree(oid)
        .map_err(|e| ApplyError::Git(format!("failed to read working tree: {e}")))
}

fn restore_tree(repo: &git2::Repository, tree: &git2::Tree<'_>) -> Result<(), ApplyError> {
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force().remove_untracked(true);
    repo.checkout_tree(tree.as_object(), Some(&mut checkout))
        .map_err(|e| ApplyError::Git(format!("failed to restore working tree: {e}")))
}

/// The numeric id of a group, given its name or the number itself, read from `/etc/group`.
pub fn resolve_group(group: &str) -> Result<u32, StageError> {
    resolve_group_in(Path::new("/etc/group"), group)
}

/// [`resolve_group`] against a group file in the usual `name:x:gid:members` format.
pub fn resolve_group_in(file: &Path, group: &str) -> Result<u32, StageError> {
    if let Ok(gid) = group.parse::<u32>() {
        return Ok(gid);
    }
    let text = fs::read_to_string(file)?;
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _password = fields.next()?;
            let gid = fields.next()?.parse::<u32>().ok()?;
            (name == group).then_some(gid)
        })
        .next()
        .ok_or_else(|| StageError::Git {
            message: format!("group {group:?} not found"),
        })
}

fn validate_work_root(work_root: &Path) -> Result<(), StageError> {
    if !work_root.is_absolute() {
        return Err(StageError::InvalidWorkRoot(
            "work root must be an absolute path".to_string(),
        ));
    }
    // Not `/` or a bare top-level directory.
    let mut components = work_root.components();
    match components.next() {
        Some(Component::RootDir) => {}
        _ => {
            return Err(StageError::InvalidWorkRoot(
                "work root must be absolute".to_string(),
            ))
        }
    }
    if work_root
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(StageError::InvalidWorkRoot(
            "work root must not contain '.' or '..' components".to_string(),
        ));
    }
    // After root, there must be at least two normal components? "not / or a bare top-level directory"
    // `/` has no normal components, `/srv` has one -> reject both? interpret as must not be / and must not be directly under / as a single component.
    let normal_count = work_root
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if normal_count <= 1 {
        return Err(StageError::InvalidWorkRoot(
            "work root must not be '/' or a bare top-level directory".to_string(),
        ));
    }
    // Must exist.
    let meta = fs::symlink_metadata(work_root)
        .map_err(|e| StageError::InvalidWorkRoot(format!("work root does not exist: {e}")))?;
    // Must not be a symlink.
    if meta.file_type().is_symlink() {
        return Err(StageError::InvalidWorkRoot(
            "work root must not be a symlink".to_string(),
        ));
    }
    if !meta.is_dir() {
        return Err(StageError::InvalidWorkRoot(
            "work root must be a directory".to_string(),
        ));
    }
    Ok(())
}

fn validate_commit_id(commit_id: &str) -> Result<git2::Oid, StageError> {
    if commit_id.len() != 40 || !commit_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StageError::InvalidCommit(commit_id.to_owned()));
    }
    git2::Oid::from_str(commit_id).map_err(|_| StageError::InvalidCommit(commit_id.to_owned()))
}

fn validate_git_ref(git_ref: &str) -> Result<(), String> {
    if git_ref.is_empty() {
        return Err("git ref must not be empty".to_string());
    }
    if git_ref.starts_with('-') {
        return Err("git ref must not start with '-'".to_string());
    }
    if git_ref.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("git ref must not contain whitespace or control characters".to_string());
    }
    Ok(())
}

fn validate_url_source(url: &str) -> Result<(), StageError> {
    let refuse = |why: &str| {
        Err(StageError::Git {
            message: format!("unsupported source url: {why}"),
        })
    };
    if url.is_empty() || url.len() > 2048 {
        return refuse("length");
    }
    if url.starts_with('-') {
        return refuse("starts with '-'");
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return refuse("whitespace or control character");
    }
    // Remote helpers such as `ext::` and `fd::` are named with `::`.
    if url.contains("::") {
        return refuse("remote helper syntax");
    }
    if url.starts_with("https://") || url.starts_with("file://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        // ssh://[user@]host[:port]/path - host must exist and not start with '-'
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = host.split(':').next().unwrap_or(host);
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if host.is_empty() {
            return refuse("missing host");
        }
        if host.starts_with('-') {
            return refuse("option-like host");
        }
        return Ok(());
    }
    if is_scp_like(url) {
        return Ok(());
    }
    refuse("scheme")
}

/// `[user@]host:path` with a plain host name and a path that cannot be read as an option.
fn is_scp_like(s: &str) -> bool {
    if s.contains("://") {
        return false;
    }
    let Some((host, path)) = s.split_once(':') else {
        return false;
    };
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
        && !host.starts_with('-')
        && !path.is_empty()
        && !path.starts_with('-')
        && !path.starts_with(':')
}

fn path_str(p: &Path) -> Result<&str, StageError> {
    p.to_str().ok_or_else(|| StageError::Git {
        message: "path is not valid utf-8".to_string(),
    })
}

fn run_git<I, S>(dir: Option<&Path>, args: I) -> Result<(), StageError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    harden(&mut cmd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(StageError::Git {
            message: format_git_failure("git", out.status, &stderr),
        })
    }
}

/// Limits what git may do on our behalf when we invoke git: only these transports (no
/// `ext::` command transport), never a terminal prompt, and no pager or external helper
/// settings from the environment.
fn harden(cmd: &mut Command) {
    cmd.env("GIT_ALLOW_PROTOCOL", "file:https:ssh")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EXTERNAL_DIFF", "")
        .env("GIT_PAGER", "cat")
        .env("GIT_CONFIG_NOSYSTEM", "1");
}

fn format_git_failure(cmd: &str, status: std::process::ExitStatus, stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("{cmd} failed with {status}")
    } else {
        format!("{cmd} failed with {status}: {stderr}")
    }
}

fn set_dir_mode_0700(path: &Path) -> Result<(), StageError> {
    set_dir_mode_0700_io(path)?;
    Ok(())
}

fn set_dir_mode_0700_io(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn cleanup_created_path(target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(target) {
        Ok(m) => {
            if m.file_type().is_symlink() || m.is_file() {
                fs::remove_file(target)
            } else {
                remove_dir_all_robust(target)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_dir_all_robust(path: &Path) -> io::Result<()> {
    // Handle read-only files and deep trees. Make contents writable before removal.
    // First try simple removal.
    match fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(_) => {
            // fall through to chmod walk
        }
    }
    make_tree_writable(path)?;
    fs::remove_dir_all(path)
}

fn make_tree_writable(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                make_tree_writable(&entry.path())?;
            }
        } else {
            let mode = meta.permissions().mode();
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o600));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
    }
    Ok(())
}

/// The three allowed artifact file kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactFile {
    DiffPatch,
    RunLog,
    ReportJson,
}

impl ArtifactFile {
    pub fn file_name(self) -> &'static str {
        match self {
            ArtifactFile::DiffPatch => "diff.patch",
            ArtifactFile::RunLog => "run.log",
            ArtifactFile::ReportJson => "report.json",
        }
    }
}

/// Controller-side artifact storage rooted at a fixed directory.
#[derive(Debug, Clone)]
pub struct ArtifactDir {
    root: PathBuf,
}

impl ArtifactDir {
    /// Create an artifact directory manager.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(ArtifactError::InvalidRoot(
                "artifact root must be an absolute path".to_string(),
            ));
        }
        let meta = fs::symlink_metadata(root).map_err(|e| {
            ArtifactError::InvalidRoot(format!("artifact root does not exist: {e}"))
        })?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(ArtifactError::InvalidRoot(
                "artifact root must be a real directory".to_string(),
            ));
        }
        Ok(ArtifactDir {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create `<root>/<job_id>` with mode 0700.
    pub fn create(&self, job_id: &str) -> Result<PathBuf, ArtifactError> {
        validate_job_id(job_id).map_err(ArtifactError::InvalidJobId)?;
        let dir = self.root.join(job_id);
        match fs::symlink_metadata(&dir) {
            Ok(_) => return Err(ArtifactError::AlreadyExists(dir)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(ArtifactError::Io(e)),
        }
        #[cfg(unix)]
        {
            let mut b = fs::DirBuilder::new();
            b.mode(0o700);
            b.create(&dir)?;
        }
        #[cfg(not(unix))]
        {
            fs::create_dir(&dir)?;
        }
        Ok(dir)
    }

    /// Write an artifact by streaming from `reader`.
    ///
    /// Writes to a temporary file in the same directory and renames it on
    /// success. Removes it when the reader exceeds the cap (`TooLarge`) or fails.
    /// Returns the number of bytes written on success.
    pub fn write<R: Read>(
        &self,
        job_id: &str,
        file: ArtifactFile,
        mut reader: R,
        cap: u64,
    ) -> Result<u64, ArtifactError> {
        validate_job_id(job_id).map_err(ArtifactError::InvalidJobId)?;
        let dir = self.root.join(job_id);
        let dir_meta = fs::symlink_metadata(&dir)?;
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            return Err(ArtifactError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "artifact job directory is not a real directory",
            )));
        }

        let final_path = dir.join(file.file_name());
        let tmp_path = temp_path_in(&dir, file.file_name());

        let result = (|| -> Result<u64, ArtifactError> {
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp_path)?;

            let mut total: u64 = 0;
            let mut buf = [0u8; 8192];
            loop {
                let n = reader.read(&mut buf).map_err(ArtifactError::Io)?;
                if n == 0 {
                    break;
                }
                total = total.saturating_add(n as u64);
                if total > cap {
                    return Err(ArtifactError::TooLarge);
                }
                f.write_all(&buf[..n]).map_err(ArtifactError::Io)?;
            }
            f.sync_all().map_err(ArtifactError::Io)?;
            fs::rename(&tmp_path, &final_path).map_err(ArtifactError::Io)?;
            Ok(total)
        })();

        if result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        result
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path_in(dir: &Path, final_name: &str) -> PathBuf {
    let pid = std::process::id();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".{final_name}.tmp.{pid}.{nanos}.{n}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("lu-staging-{name}-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn source_repo(root: &Path) -> PathBuf {
        let repo = root.join("source");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &["config", "--local", "user.email", "test@example.net"],
        );
        git(&repo, &["config", "--local", "user.name", "Test"]);
        git(&repo, &["config", "--local", "commit.gpgsign", "false"]);
        fs::write(repo.join("README.md"), "test\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        repo
    }

    fn patch_from(repo: &Path, content: &str) -> Vec<u8> {
        fs::write(repo.join("README.md"), content).unwrap();
        let out = Command::new("git")
            .args(["diff", "--no-color", "--binary", "HEAD", "--", "README.md"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(out.status.success());
        fs::write(repo.join("README.md"), "test\n").unwrap();
        out.stdout
    }

    #[test]
    fn a_failed_merge_leaves_the_worktree_unchanged() {
        let tmp = TestDir::new("conflict");
        let repo = source_repo(&tmp.path);
        let patch = patch_from(&repo, "two\n");
        let work = tmp.path.join("work");
        fs::create_dir(&work).unwrap();
        let staging = Staging::new(&work).unwrap();
        let staged = staging
            .stage(StageRequest {
                job_id: "job".to_owned(),
                source: Source::Path(repo),
                git_ref: "main".to_owned(),
            })
            .unwrap();
        fs::write(staged.path().join("README.md"), "one\n").unwrap();

        let error = Staging::apply_dependency_diff(&staged, &patch).unwrap_err();
        match error {
            ApplyError::Conflict { paths, .. } => assert_eq!(paths, ["README.md"]),
            other => panic!("expected a conflict, got {other}"),
        }
        assert_eq!(
            fs::read_to_string(staged.path().join("README.md")).unwrap(),
            "one\n"
        );
    }

    #[test]
    fn export_diff_never_contains_the_local_inputs_directory() {
        let tmp = TestDir::new("inputs-diff");
        let repo = source_repo(&tmp.path);
        let work = tmp.path.join("work");
        fs::create_dir(&work).unwrap();
        let staging = Staging::new(&work).unwrap();
        let staged = staging
            .stage(StageRequest {
                job_id: "job".to_owned(),
                source: Source::Path(repo),
                git_ref: "main".to_owned(),
            })
            .unwrap();

        let inputs = prepare_inputs(&staged).unwrap();
        let dep = create_input_dir(&inputs, "dep").unwrap();
        write_input_file(&dep, "diff.patch", b"not a real patch\n").unwrap();
        fs::write(staged.path().join("new.txt"), "job output\n").unwrap();

        let diff = String::from_utf8(Staging::export_diff(&staged, 1 << 20).unwrap()).unwrap();
        assert!(diff.contains("new.txt"), "{diff}");
        assert!(!diff.contains(".lu"), "{diff}");
        assert!(!diff.contains("not a real patch"), "{diff}");

        let exclude = fs::read_to_string(staged.path().join(".git/info/exclude")).unwrap();
        assert!(
            exclude.lines().any(|line| line.trim() == ".lu/"),
            "{exclude}"
        );
        assert!(!staged.path().join(".gitignore").exists());
    }

    #[test]
    fn export_diff_with_paths_lists_exactly_the_uploaded_changes() {
        let tmp = TestDir::new("export-paths");
        let repo = source_repo(&tmp.path);
        let work = tmp.path.join("work");
        fs::create_dir(&work).unwrap();
        let staging = Staging::new(&work).unwrap();
        let staged = staging
            .stage(StageRequest {
                job_id: "job".to_owned(),
                source: Source::Path(repo),
                git_ref: "main".to_owned(),
            })
            .unwrap();

        fs::write(staged.path().join("README.md"), "changed\n").unwrap();
        fs::write(staged.path().join("new.txt"), "output\n").unwrap();
        let inputs = prepare_inputs(&staged).unwrap();
        let dep = create_input_dir(&inputs, "dep").unwrap();
        write_input_file(&dep, "diff.patch", b"not a real patch\n").unwrap();

        let exported = Staging::export_diff_with_paths(&staged, 1 << 20).unwrap();
        assert!(exported.changed_paths.iter().any(|p| p == "README.md"));
        assert!(exported.changed_paths.iter().any(|p| p == "new.txt"));
        assert!(
            exported
                .changed_paths
                .iter()
                .all(|p| !is_lu_path(Path::new(p))),
            "{:?}",
            exported.changed_paths
        );
        assert_eq!(
            exported.bytes,
            Staging::export_diff(&staged, 1 << 20).unwrap()
        );
    }

    #[test]
    fn a_tracked_lu_path_is_a_stage_error() {
        let tmp = TestDir::new("tracked-lu");
        let repo = tmp.path.join("source");
        source_repo(&tmp.path);
        git(&repo, &["checkout", "-q", "-b", "with-lu"]);
        fs::create_dir(repo.join(".lu")).unwrap();
        fs::write(repo.join(".lu/tracked.txt"), "secret\n").unwrap();
        git(&repo, &["add", ".lu/tracked.txt"]);
        git(&repo, &["commit", "-q", "-m", "track lu"]);

        let work = tmp.path.join("work");
        fs::create_dir(&work).unwrap();
        let staging = Staging::new(&work).unwrap();
        let error = staging
            .stage(StageRequest {
                job_id: "job".to_owned(),
                source: Source::Path(repo),
                git_ref: "with-lu".to_owned(),
            })
            .unwrap_err();
        assert!(matches!(error, StageError::TrackedLuPath(_)), "{error}");
        assert!(error.to_string().contains(".lu/"), "{error}");
    }

    #[test]
    fn collected_files_are_private_and_symlinks_are_refused() {
        let tmp = TestDir::new("inputs-modes");
        let repo = source_repo(&tmp.path);
        let work = tmp.path.join("work");
        fs::create_dir(&work).unwrap();
        let staging = Staging::new(&work).unwrap();
        let staged = staging
            .stage(StageRequest {
                job_id: "job".to_owned(),
                source: Source::Path(repo),
                git_ref: "main".to_owned(),
            })
            .unwrap();

        let inputs = prepare_inputs(&staged).unwrap();
        let dep = create_input_dir(&inputs, "dep:1").unwrap();
        write_input_file(&dep, "run.log", b"tail\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&inputs).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{mode:o}");
            let mode = fs::metadata(dep.join("run.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }

        let hostile = create_input_dir(&inputs, "../../x").unwrap_err();
        assert!(
            matches!(hostile, StageError::UnsafeInputPath(_)),
            "{hostile}"
        );

        let link = tmp.path.join("outside");
        fs::write(&link, "outside\n").unwrap();
        let another = staging
            .stage(StageRequest {
                job_id: "linked".to_owned(),
                source: Source::Path(source_repo(&tmp.path.join("other"))),
                git_ref: "main".to_owned(),
            })
            .unwrap();
        std::os::unix::fs::symlink(&link, another.path().join(".lu")).unwrap();
        let error = prepare_inputs(&another).unwrap_err();
        assert!(matches!(error, StageError::UnsafeInputPath(_)), "{error}");
    }

    #[test]
    fn safe_input_names_fall_back_to_the_job_id() {
        assert_eq!(safe_input_name(Some("ticket:1234"), "job"), "ticket:1234");
        assert_eq!(safe_input_name(Some("../../x"), "job-1"), "job-1");
        assert_eq!(safe_input_name(Some("bad name"), "job-2"), "job-2");
        assert_eq!(safe_input_name(None, "job-3"), "job-3");
    }
}
