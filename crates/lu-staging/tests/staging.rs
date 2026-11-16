use lu_staging::{ArtifactDir, ArtifactFile, JobId, Source, StageError, StageRequest, Staging};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let base = PathBuf::from("/tmp/opencode");
    let base = if base.is_dir() {
        base
    } else {
        std::env::temp_dir()
    };
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for i in 0..1000u64 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed) + i;
        let p = base.join(format!("lu-staging-{prefix}-{pid}-{nanos}-{n}"));
        match fs::create_dir(&p) {
            Ok(()) => return p,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("failed to create temp dir {}: {e}", p.display()),
        }
    }
    panic!("could not create unique temp dir");
}

fn run(dir: Option<&Path>, args: &[&str]) -> String {
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..]);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().expect("command spawn failed");
    if !out.status.success() {
        panic!(
            "command {:?} failed: {}\nstdout: {}\nstderr: {}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn init_repo() -> PathBuf {
    let dir = unique_temp_dir("repo");
    run(None, &["git", "init", "-b", "main", dir.to_str().unwrap()]);
    run(
        Some(&dir),
        &["git", "config", "user.email", "test@example.net"],
    );
    run(Some(&dir), &["git", "config", "user.name", "Test"]);
    fs::write(dir.join("hello.txt"), "hello\n").unwrap();
    run(Some(&dir), &["git", "add", "hello.txt"]);
    run(Some(&dir), &["git", "commit", "-m", "init"]);
    dir
}

fn make_work_root() -> PathBuf {
    // Must not be / or bare top-level dir, and must exist and not be symlink.
    let base = unique_temp_dir("workbase");
    let root = base.join("work");
    fs::create_dir(&root).unwrap();
    root
}

macro_rules! require_git {
    () => {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
    };
}

#[test]
fn stage_from_local_repository() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-1".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    assert!(staged.path().is_dir());
    assert_eq!(staged.job_id(), "job-1");
    assert_eq!(
        fs::read_to_string(staged.path().join("hello.txt")).unwrap(),
        "hello\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(staged.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn stage_from_file_url() {
    require_git!();
    let repo = init_repo();
    let url = format!("file://{}", repo.display());
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-fileurl".to_string(),
            source: Source::Url(url),
            git_ref: "main".to_string(),
        })
        .unwrap();
    assert!(staged.path().join("hello.txt").is_file());
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn stage_ref_with_leading_dash_rejected() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let err = staging
        .stage(StageRequest {
            job_id: "job-badref".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "-bad".to_string(),
        })
        .unwrap_err();
    assert!(matches!(err, StageError::InvalidRef(_)));
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn invalid_job_ids_rejected() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    for bad in ["..", "a/b", "", &"a".repeat(65)] {
        let err = staging
            .stage(StageRequest {
                job_id: bad.to_string(),
                source: Source::Path(repo.clone()),
                git_ref: "main".to_string(),
            })
            .unwrap_err();
        assert!(
            matches!(err, StageError::InvalidJobId(_)),
            "expected invalid for {bad:?}"
        );
    }
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn second_stage_same_id_errors() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    staging
        .stage(StageRequest {
            job_id: "job-dupe".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    let err = staging
        .stage(StageRequest {
            job_id: "job-dupe".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap_err();
    assert!(matches!(err, StageError::AlreadyExists(_)));
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn failed_clone_leaves_nothing() {
    require_git!();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let missing = root.join("no-such-repo");
    let err = staging
        .stage(StageRequest {
            job_id: "job-fail".to_string(),
            source: Source::Path(missing),
            git_ref: "main".to_string(),
        })
        .unwrap_err();
    assert!(matches!(err, StageError::Git { .. }));
    assert!(!root.join("job-fail").exists());
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn export_diff_changes_and_ignored() {
    require_git!();
    let repo = init_repo();
    // add a file to be deleted, and a binary file to modify context
    fs::write(repo.join("gone.txt"), "bye\n").unwrap();
    fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, 255]).unwrap();
    run(Some(&repo), &["git", "add", "."]);
    run(Some(&repo), &["git", "commit", "-m", "more"]);
    fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
    run(Some(&repo), &["git", "add", ".gitignore"]);
    run(Some(&repo), &["git", "commit", "-m", "ignore"]);

    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-diff".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();

    // added, modified, deleted, binary, and ignored file
    fs::write(staged.path().join("added.txt"), "new\n").unwrap();
    fs::write(staged.path().join("hello.txt"), "changed\n").unwrap();
    fs::remove_file(staged.path().join("gone.txt")).unwrap();
    fs::write(staged.path().join("bin.dat"), [0u8, 1, 2, 4, 255, 9]).unwrap();
    fs::write(staged.path().join("ignored.txt"), "secret\n").unwrap();

    let patch = Staging::export_diff(&staged, 1024 * 1024).unwrap();
    let patch_str = String::from_utf8_lossy(&patch);
    assert!(
        patch_str.contains("added.txt"),
        "patch missing added: {patch_str}"
    );
    assert!(
        patch_str.contains("hello.txt"),
        "patch missing modified: {patch_str}"
    );
    assert!(
        patch_str.contains("gone.txt"),
        "patch missing deleted: {patch_str}"
    );
    assert!(
        patch_str.contains("bin.dat"),
        "patch missing binary: {patch_str}"
    );
    assert!(
        patch_str.contains("GIT binary patch") || patch_str.contains("Binary files"),
        "patch not binary-safe: {patch_str}"
    );
    assert!(
        !patch_str.contains("ignored.txt"),
        "ignored file included: {patch_str}"
    );

    let err = Staging::export_diff(&staged, 10).unwrap_err();
    assert!(matches!(err, StageError::TooLarge));

    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn clean_symlinked_job_dir_rejected() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let outside = unique_temp_dir("outside");
    std::os::unix::fs::symlink(&outside, root.join("job-link")).unwrap();
    let err = staging.clean("job-link").unwrap_err();
    assert!(matches!(
        err,
        StageError::Git { .. } | StageError::InvalidJobId(_)
    ));
    assert!(outside.exists(), "outside dir must remain");
    let _ = staging; // suppress warning
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
    let _ = fs::remove_dir_all(outside);
}

#[cfg(unix)]
#[test]
fn clean_with_symlink_inside_pointing_outside() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-insymlink".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    let outside = unique_temp_dir("outside2");
    fs::write(outside.join("keep.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, staged.path().join("escape")).unwrap();
    staging.clean("job-insymlink").unwrap();
    assert!(!root.join("job-insymlink").exists());
    assert!(
        outside.join("keep.txt").exists(),
        "symlink target content must remain"
    );
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn clean_readonly_file_missing_and_clean_all_except() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    // readonly file inside staged dir
    let s1 = staging
        .stage(StageRequest {
            job_id: "job-ro".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    let ro = s1.path().join("readonly.txt");
    fs::write(&ro, "x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o400)).unwrap();
    }
    staging.clean("job-ro").unwrap();
    assert!(!root.join("job-ro").exists());

    // missing directory is not an error
    staging.clean("job-missing").unwrap();

    // clean_all_except
    staging
        .stage(StageRequest {
            job_id: "keep-me".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    staging
        .stage(StageRequest {
            job_id: "drop-me".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    // something else that must be left alone
    fs::create_dir(root.join("not_a_job!")).unwrap();
    fs::write(root.join("note.txt"), "hi").unwrap();
    let keep = vec![JobId::new("keep-me").unwrap()];
    staging.clean_all_except(&keep).unwrap();
    assert!(root.join("keep-me").exists());
    assert!(!root.join("drop-me").exists());
    assert!(root.join("not_a_job!").exists());
    assert!(root.join("note.txt").exists());

    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn staging_new_validations() {
    let base = unique_temp_dir("rootval");
    // not absolute
    assert!(Staging::new("relative/path").is_err());
    // root /
    assert!(Staging::new("/").is_err());
    // bare top-level
    assert!(Staging::new("/srv").is_err());
    // non-existent
    assert!(Staging::new(base.join("nope")).is_err());
    #[cfg(unix)]
    {
        let real = base.join("real");
        fs::create_dir_all(&real).unwrap();
        // symlink root
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(Staging::new(&link).is_err());
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn artifact_dir_create_write_cap_rename_and_no_partial() {
    let base = unique_temp_dir("artbase");
    let root = base.join("artifacts");
    fs::create_dir(&root).unwrap();
    let ad = ArtifactDir::new(&root).unwrap();

    let dir = ad.create("job-art").unwrap();
    assert!(dir.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    // duplicate
    assert!(ad.create("job-art").is_err());

    let n = ad
        .write(
            "job-art",
            ArtifactFile::DiffPatch,
            Cursor::new(b"patch"),
            100,
        )
        .unwrap();
    assert_eq!(n, 5);
    assert_eq!(fs::read(dir.join("diff.patch")).unwrap(), b"patch");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("diff.patch"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    // rename/overwrite behavior not specified but writing again to same name: temp+rename replaces; ensure it works
    let n = ad
        .write(
            "job-art",
            ArtifactFile::RunLog,
            Cursor::new(b"logdata"),
            100,
        )
        .unwrap();
    assert_eq!(n, 7);
    assert_eq!(fs::read(dir.join("run.log")).unwrap(), b"logdata");

    // cap exceeded leaves no partial file
    let err = ad
        .write(
            "job-art",
            ArtifactFile::ReportJson,
            Cursor::new(b"0123456789"),
            5,
        )
        .unwrap_err();
    assert!(matches!(err, lu_staging::ArtifactError::TooLarge));
    assert!(!dir.join("report.json").exists());
    // and no temp leftovers
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "left temp files: {leftovers:?}");

    // failed reader leaves no partial file
    struct FailReader;
    impl std::io::Read for FailReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("boom"))
        }
    }
    let err = ad
        .write("job-art", ArtifactFile::ReportJson, FailReader, 100)
        .unwrap_err();
    assert!(matches!(err, lu_staging::ArtifactError::Io(_)));
    assert!(!dir.join("report.json").exists());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn urls_that_would_run_a_command_or_inject_an_option_are_refused() {
    require_git!();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let marker = root.join("pwned");
    let hostile = [
        format!("ext::sh -c touch% {}", marker.display()),
        "ext::sh -c id".to_string(),
        "fd::3".to_string(),
        "--upload-pack=touch x".to_string(),
        "-oProxyCommand=id".to_string(),
        "host:--upload-pack=id".to_string(),
        "http://example.net/x.git".to_string(),
        "ftp://example.net/x.git".to_string(),
        "https://example.net/x\ny".to_string(),
    ];
    for (i, url) in hostile.iter().enumerate() {
        let err = staging
            .stage(StageRequest {
                job_id: format!("job-hostile-{i}"),
                source: Source::Url(url.clone()),
                git_ref: "main".to_string(),
            })
            .expect_err(url);
        assert!(!marker.exists(), "{url} ran a command");
        let _ = err;
        assert!(!root.join(format!("job-hostile-{i}")).exists());
    }
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn ssh_url_host_validation() {
    require_git!();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    for (i, url) in [
        "ssh://-oProxyCommand=x/repo",
        "ssh://user@-host/repo",
        "ssh:///repo",
        "ssh://user@/repo",
    ]
    .into_iter()
    .enumerate()
    {
        let err = staging
            .stage(StageRequest {
                job_id: format!("job-sshbad-{i}"),
                source: Source::Url(url.to_string()),
                git_ref: "main".to_string(),
            })
            .expect_err(url);
        assert!(
            matches!(err, StageError::Git { .. }),
            "expected git/url error for {url}: {err:?}"
        );
    }
    // A syntactically valid ssh URL passes source validation (clone may fail, but not as InvalidRef/InvalidJobId).
    // Use a non-resolving host; we only assert it was not rejected as an option-like/missing host.
    let err = staging
        .stage(StageRequest {
            job_id: "job-sshgood".to_string(),
            source: Source::Url("ssh://host/repo".to_string()),
            git_ref: "main".to_string(),
        })
        .unwrap_err();
    assert!(
        matches!(err, StageError::Git { .. }),
        "expected clone git error, got {err:?}"
    );
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn a_local_path_must_be_absolute() {
    require_git!();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    assert!(staging
        .stage(StageRequest {
            job_id: "job-rel-0".to_string(),
            source: Source::Path(PathBuf::from("relative/repo")),
            git_ref: "main".to_string(),
        })
        .is_err());
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn local_clone_does_not_share_objects() {
    require_git!();
    let repo = init_repo();
    // create some objects/content
    fs::write(repo.join("more.txt"), "more\n").unwrap();
    run(Some(&repo), &["git", "add", "more.txt"]);
    run(Some(&repo), &["git", "commit", "-m", "more"]);
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-nolocal".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fn walk(dir: &Path) -> Vec<PathBuf> {
            let mut out = Vec::new();
            if let Ok(entries) = fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        out.extend(walk(&p));
                    } else {
                        out.push(p);
                    }
                }
            }
            out
        }
        for p in walk(&staged.path().join(".git").join("objects")) {
            if let Ok(m) = fs::metadata(&p) {
                if m.is_file() {
                    assert!(
                        m.nlink() <= 1,
                        "shared object {} nlink {}",
                        p.display(),
                        m.nlink()
                    );
                }
            }
        }
    }
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn export_diff_does_not_run_external_programs_and_handles_large_output() {
    require_git!();
    let repo = init_repo();
    // a file to produce large diff and warnings context not necessary; "more than 64 KiB of warnings or output does not hang"
    // Create many changes to produce large output.
    fs::write(repo.join("big.txt"), "a\n".repeat(2000)).unwrap();
    run(Some(&repo), &["git", "add", "big.txt"]);
    run(Some(&repo), &["git", "commit", "-m", "big"]);
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-noprogs".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();

    let marker_dir = root.join("markers");
    fs::create_dir(&marker_dir).unwrap();
    let marker_fsmonitor = marker_dir.join("fsmonitor");
    let marker_external = marker_dir.join("external");
    let marker_clean = marker_dir.join("clean");
    // commands that create markers
    let touch = |p: &Path| format!("sh -c 'echo x > {}'", p.display());
    run(
        Some(staged.path()),
        &["git", "config", "core.fsmonitor", &touch(&marker_fsmonitor)],
    );
    run(
        Some(staged.path()),
        &["git", "config", "diff.external", &touch(&marker_external)],
    );
    run(
        Some(staged.path()),
        &[
            "git",
            "config",
            "filter.marker.clean",
            &touch(&marker_clean),
        ],
    );
    run(
        Some(staged.path()),
        &["git", "config", "filter.marker.smudge", "cat"],
    );
    fs::write(
        staged.path().join(".gitattributes"),
        "filt.txt filter=marker\n",
    )
    .unwrap();
    fs::write(staged.path().join("filt.txt"), "data\n").unwrap();
    // make a large change
    fs::write(staged.path().join("big.txt"), "b\n".repeat(2000)).unwrap();

    let diff = Staging::export_diff(&staged, 4 * 1024 * 1024).unwrap();
    assert!(!diff.is_empty());
    assert!(!marker_fsmonitor.exists(), "core.fsmonitor ran");
    assert!(!marker_external.exists(), "diff.external ran");
    assert!(!marker_clean.exists(), "clean filter ran");

    // Large output path: cap smaller than diff should return TooLarge without hanging.
    let err = Staging::export_diff(&staged, 1024).unwrap_err();
    assert!(matches!(err, StageError::TooLarge));

    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn artifact_temp_removed_on_failure_modes_checked() {
    let base = unique_temp_dir("artbase2");
    let root = base.join("artifacts");
    fs::create_dir(&root).unwrap();
    let ad = ArtifactDir::new(&root).unwrap();
    let dir = ad.create("job-art2").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    // failure leaves no temp
    struct FailReader;
    impl std::io::Read for FailReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf[0] = b'x';
            Err(std::io::Error::other("boom"))
        }
    }
    let _ = ad
        .write("job-art2", ArtifactFile::RunLog, FailReader, 100)
        .unwrap_err();
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "left temp files: {leftovers:?}");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn clean_all_except_only_removes_directories_that_stage_made() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    staging
        .stage(StageRequest {
            job_id: "staged-one".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    // A directory with a perfectly valid job-id-like name that stage did not create, as would
    // sit in a work root that was pointed at the wrong place.
    for name in ["Documents", "src", "project-2"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("keep.txt"), "mine").unwrap();
    }
    staging.clean_all_except(&[]).unwrap();
    assert!(!root.join("staged-one").exists());
    for name in ["Documents", "src", "project-2"] {
        assert!(
            root.join(name).join("keep.txt").exists(),
            "{name} was removed"
        );
    }
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn a_work_root_with_dot_components_is_refused() {
    let base = unique_temp_dir("dots");
    let root = base.join("work");
    fs::create_dir(&root).unwrap();
    // A trailing `.` is dropped by `Path::components`, so it is the root itself.
    for path in [root.join("..").join("work"), base.join("work/../work")] {
        assert!(Staging::new(&path).is_err(), "{}", path.display());
    }
    assert!(Staging::new(&root).is_ok());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn the_exported_diff_does_not_run_external_diff_or_textconv_drivers() {
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-drivers".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    // A harness could edit the checkout's own config; the driver must not run.
    let proof = root.join("driver-ran");
    run(
        Some(staged.path()),
        &[
            "git",
            "config",
            "diff.external",
            &format!("touch {}", proof.display()),
        ],
    );
    fs::write(staged.path().join("hello.txt"), "changed\n").unwrap();
    let diff = Staging::export_diff(&staged, 1 << 20).unwrap();
    assert!(String::from_utf8_lossy(&diff).contains("changed"));
    assert!(!proof.exists(), "diff.external ran");
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn sharing_with_a_group_opens_the_tree_but_never_git_or_symlink_targets() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    require_git!();
    let repo = init_repo();
    let root = make_work_root();
    let staging = Staging::new(&root).unwrap();
    let staged = staging
        .stage(StageRequest {
            job_id: "job-share".to_string(),
            source: Source::Path(repo.clone()),
            git_ref: "main".to_string(),
        })
        .unwrap();
    let outside = root.parent().unwrap().join("outside");
    fs::write(&outside, "keep").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&outside, staged.path().join("link")).unwrap();
    fs::create_dir(staged.path().join("sub")).unwrap();
    fs::write(staged.path().join("sub/run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        staged.path().join("sub/run.sh"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let git_before = fs::metadata(staged.path().join(".git")).unwrap().mode();

    // The caller's own group: changing to it needs no privilege, and the mode bits are the test.
    let gid = fs::metadata(staged.path()).unwrap().gid();
    staging.share_with_group(&staged, gid).unwrap();

    let mode = |p: &str| fs::symlink_metadata(staged.path().join(p)).unwrap().mode() & 0o7777;
    assert_eq!(
        fs::metadata(staged.path()).unwrap().mode() & 0o7777,
        0o2770,
        "the checkout is setgid, group read and write"
    );
    assert_eq!(
        mode("hello.txt") & 0o060,
        0o060,
        "tracked files are group writable"
    );
    assert_eq!(
        mode("sub") & 0o2070,
        0o2070,
        "new directories inherit the group"
    );
    assert_eq!(
        mode("sub/run.sh") & 0o070,
        0o070,
        "an executable stays group executable"
    );
    assert_eq!(
        fs::metadata(staged.path().join(".git")).unwrap().mode(),
        git_before,
        ".git is untouched"
    );
    assert_eq!(
        fs::metadata(&outside).unwrap().mode() & 0o7777,
        0o600,
        "a symlink's target is not followed"
    );
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn a_group_is_found_by_name_or_number() {
    let dir = unique_temp_dir("groups");
    let file = dir.join("group");
    fs::write(
        &file,
        "root:x:0:\nopencode:x:1001:root\nbad line\nstaff:x:notanumber:\n",
    )
    .unwrap();
    assert_eq!(
        lu_staging::resolve_group_in(&file, "opencode").unwrap(),
        1001
    );
    assert_eq!(lu_staging::resolve_group_in(&file, "root").unwrap(), 0);
    assert_eq!(lu_staging::resolve_group_in(&file, "4242").unwrap(), 4242);
    assert!(lu_staging::resolve_group_in(&file, "staff").is_err());
    assert!(lu_staging::resolve_group_in(&file, "nobody-here").is_err());
    let _ = fs::remove_dir_all(dir);
}
