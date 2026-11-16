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

/// Error returned by staging, diff export and cleanup operations.
#[derive(Debug)]
pub enum StageError {
    /// The configured work root is invalid.
    InvalidWorkRoot(String),
    /// A job id did not satisfy the validation rules.
    InvalidJobId(String),
    /// A git ref was refused.
    InvalidRef(String),
    /// The target directory already exists.
    AlreadyExists(PathBuf),
    /// I/O failure.
    Io(io::Error),
    /// A git command failed.
    Git { message: String },
    /// The produced diff exceeded the configured cap.
    TooLarge,
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageError::InvalidWorkRoot(msg) => write!(f, "invalid work root: {msg}"),
            StageError::InvalidJobId(msg) => write!(f, "invalid job id: {msg}"),
            StageError::InvalidRef(msg) => write!(f, "invalid git ref: {msg}"),
            StageError::AlreadyExists(p) => write!(f, "directory already exists: {}", p.display()),
            StageError::Io(e) => write!(f, "io error: {e}"),
            StageError::Git { message } => write!(f, "git error: {message}"),
            StageError::TooLarge => write!(f, "output exceeded size cap"),
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

/// Where to obtain the repository for a staged job.
#[derive(Debug, Clone)]
pub enum Source {
    /// `https://`, `ssh://`, `file://` URLs or scp-style `host:path`.
    Url(String),
    /// An absolute path to a local repository.
    Path(PathBuf),
    /// A git bundle file.
    Bundle(PathBuf),
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

/// Manages per-job staging directories under a fixed work root.
/// The file inside `.git` that says this crate staged the directory.
const STAGED_MARKER: &str = "lu-staged";

#[derive(Debug, Clone)]
pub struct Staging {
    work_root: PathBuf,
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
        if let Err(e) = result {
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
                        "--",
                        path_str(path)?,
                        path_str(target)?,
                    ],
                )?;
            }
            Source::Bundle(path) => {
                // A relative path could start with `-` and be read as an option.
                if !path.is_absolute() {
                    return Err(StageError::Git {
                        message: "bundle path must be absolute".to_string(),
                    });
                }
                run_git(
                    None,
                    [
                        "clone",
                        "--no-checkout",
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
    /// including new files, as a binary-safe patch.
    pub fn export_diff(staged: &StagedDir, cap_bytes: usize) -> Result<Vec<u8>, StageError> {
        // Stage intent for untracked, non-ignored files so they appear in diff.
        run_git(Some(staged.path()), ["add", "-N", "."])?;

        let mut command = Command::new("git");
        harden(&mut command);
        let mut child = command
            .args([
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
            ])
            .current_dir(staged.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdout = child.stdout.take().ok_or_else(|| StageError::Git {
            message: "failed to capture git diff stdout".to_string(),
        })?;
        let stderr_handle = child.stderr.take();

        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let mut too_large = false;
        loop {
            let n = stdout.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if out.len().saturating_add(n) > cap_bytes {
                too_large = true;
                // Stop reading early; do not buffer unbounded output.
                let _ = child.kill();
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        // Drain remaining pipe best-effort after kill to avoid blocking on wait in some cases.
        if too_large {
            let mut sink = [0u8; 4096];
            while stdout.read(&mut sink).ok().unwrap_or(0) > 0 {}
        }

        let status = child.wait()?;
        let mut stderr = String::new();
        if let Some(mut s) = stderr_handle {
            let _ = s.read_to_string(&mut stderr);
        }

        if too_large {
            return Err(StageError::TooLarge);
        }
        if !status.success() {
            return Err(StageError::Git {
                message: format_git_failure("git diff --binary", status, &stderr),
            });
        }
        Ok(out)
    }

    /// Remove `<work_root>/<job_id>` safely.
    ///
    /// A missing directory is not an error.
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
    if url.starts_with("https://") || url.starts_with("ssh://") || url.starts_with("file://") {
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

/// Limits what git may do on our behalf: only these transports (no `ext::` command transport),
/// never a terminal prompt, and no pager or external helper settings from the environment.
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

fn set_file_mode_0600(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
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
        fs::create_dir(&dir)?;
        set_dir_mode_0700_io(&dir)?;
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
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            // Best effort restrictive mode on create.
            let _ = set_file_mode_0600(&tmp_path);

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
            set_file_mode_0600(&tmp_path).map_err(ArtifactError::Io)?;
            fs::rename(&tmp_path, &final_path).map_err(ArtifactError::Io)?;
            set_file_mode_0600(&final_path).map_err(ArtifactError::Io)?;
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
