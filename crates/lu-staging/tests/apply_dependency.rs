//! Applying an exported `diff.patch` to a fresh checkout of the base commit.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lu_staging::{
    create_input_dir, prepare_inputs, write_input_file, ApplyError, ConflictPolicy, Source,
    StageRequest, StagedDir, Staging, INPUTS_DIR,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("lu-apply-dependency-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn base_repo(tmp: &TestDir, content: &str) -> PathBuf {
    base_repo_with(tmp, &[("docs.md", content)])
}

fn base_repo_with(tmp: &TestDir, files: &[(&str, &str)]) -> PathBuf {
    let repo = tmp.0.join("source");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "--local", "user.email", "t@e.net"]);
    git(&repo, &["config", "--local", "user.name", "T"]);
    for (path, content) in files {
        let full = repo.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
        git(&repo, &["add", path]);
    }
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn stage(staging: &Staging, repo: &Path, id: &str) -> StagedDir {
    staging
        .stage(StageRequest {
            job_id: id.into(),
            source: Source::Path(repo.to_path_buf()),
            git_ref: "main".into(),
        })
        .unwrap()
}

fn export(producer: &StagedDir, changed: &str) -> Vec<u8> {
    fs::write(producer.path().join("docs.md"), changed).unwrap();
    Staging::export_diff(producer, 1 << 20).unwrap()
}

/// Generates one independent input patch from a fresh checkout of the base files.
fn export_change(
    staging: &Staging,
    repo: &Path,
    id: &str,
    base: &[(&str, &str)],
    path: &str,
    changed: &str,
) -> Vec<u8> {
    let producer = stage(staging, repo, id);
    for (file, content) in base {
        fs::write(producer.path().join(file), content).unwrap();
    }
    fs::write(producer.path().join(path), changed).unwrap();
    Staging::export_diff(&producer, 1 << 20).unwrap()
}

fn table(rows: usize) -> String {
    let mut content = String::from("# Design\n\n");
    for i in 0..rows {
        content.push_str(&format!("| Feature {i:<5} | yes | note {i:<4} |\n"));
    }
    content
}

/// A one-row Markdown table change, exported by libgit2 and applied to a fresh checkout of the
/// base commit, must produce exactly the producer's post-image.
#[test]
fn a_markdown_table_row_change_applies() {
    let tmp = TestDir::new("table");
    let original = format!(
        "{}| Tenant middleware | planned | needs review |\n",
        table(38)
    );
    let repo = base_repo(&tmp, &original);
    let work = tmp.0.join("work");
    fs::create_dir(&work).unwrap();
    let staging = Staging::new(&work).unwrap();
    let producer = stage(&staging, &repo, "producer");
    let changed = original.replacen("planned", "done   ", 1);
    let patch = export(&producer, &changed);

    let consumer = stage(&staging, &repo, "consumer");
    Staging::apply_dependency_diff(&consumer, &patch).unwrap();
    assert_eq!(
        fs::read_to_string(consumer.path().join("docs.md")).unwrap(),
        changed
    );
}

/// libgit2 requires an exact hunk offset; git searches for the real position. A patch with a
/// shifted hunk header that git accepts must still apply through the fallback chain.
#[test]
fn a_shifted_hunk_header_falls_back_to_git() {
    if !git_available() {
        eprintln!("skipping git fallback test: git is not on PATH");
        return;
    }
    let tmp = TestDir::new("offset");
    let row = "| Tenant middleware | planned | needs review |\n";
    let original = format!("{}{row}{row}", table(38));
    let repo = base_repo(&tmp, &original);
    let work = tmp.0.join("work");
    fs::create_dir(&work).unwrap();
    let staging = Staging::new(&work).unwrap();
    let producer = stage(&staging, &repo, "producer");
    let changed = original.replacen("planned", "done   ", 1);
    let patch = export(&producer, &changed);
    let text = String::from_utf8(patch).unwrap();
    let shifted = text.replacen("@@ -38,5 +38,5 @@", "@@ -38,5 +39,5 @@", 1);
    assert_ne!(shifted, text, "hunk header not found");

    let consumer = stage(&staging, &repo, "consumer");
    Staging::apply_dependency_diff(&consumer, shifted.as_bytes()).unwrap();
    assert_eq!(
        fs::read_to_string(consumer.path().join("docs.md")).unwrap(),
        changed
    );
}

/// When every strategy fails, the conflict names the file and hunk and leaves the checkout
/// untouched.
#[test]
fn an_impossible_patch_reports_the_file_and_hunk() {
    let tmp = TestDir::new("impossible");
    let original = "| Tenant middleware | planned | needs review |\n";
    let repo = base_repo(&tmp, original);
    let work = tmp.0.join("work");
    fs::create_dir(&work).unwrap();
    let staging = Staging::new(&work).unwrap();
    let producer = stage(&staging, &repo, "producer");
    let patch = export(&producer, "| Tenant middleware | done   | needs review |\n");
    // Corrupt the context so no strategy can apply the patch.
    let text = String::from_utf8(patch)
        .unwrap()
        .replace("needs review", "different text");

    let consumer = stage(&staging, &repo, "consumer");
    let error = Staging::apply_dependency_diff(&consumer, text.as_bytes()).unwrap_err();
    match error {
        ApplyError::Conflict { paths, detail } => {
            assert_eq!(paths, ["docs.md"]);
            let detail = detail.unwrap_or_default();
            assert!(detail.contains("docs.md"), "{detail}");
            assert!(detail.contains("hunk at line"), "{detail}");
        }
        other => panic!("expected a conflict, got {other}"),
    }
    assert_eq!(
        fs::read_to_string(consumer.path().join("docs.md")).unwrap(),
        original
    );
}

struct ThreeInputs {
    consumer: StagedDir,
    first: Vec<u8>,
    second: Vec<u8>,
    third: Vec<u8>,
    docs: String,
    notes: &'static str,
}

/// Three inputs from the same base: the first is clean, the second conflicts with it, and the
/// third touches a different file so it can still apply after either conflict policy.
fn three_inputs(tmp: &TestDir) -> ThreeInputs {
    let docs = concat!(
        "# Design\n\n",
        "Feature A: old\n",
        "filler 1\n",
        "filler 2\n",
        "filler 3\n",
        "filler 4\n",
        "Feature B: old\n",
        "end\n",
    )
    .to_owned();
    let notes = "Notes: old\n";
    let base = [("docs.md", docs.as_str()), ("notes.md", notes)];
    let repo = base_repo_with(tmp, &base);
    let work = tmp.0.join("work");
    fs::create_dir(&work).unwrap();
    let staging = Staging::new(&work).unwrap();
    let first = export_change(
        &staging,
        &repo,
        "p1",
        &base,
        "docs.md",
        &docs.replacen("Feature A: old", "Feature A: one", 1),
    );
    let second = export_change(
        &staging,
        &repo,
        "p2",
        &base,
        "docs.md",
        &docs.replacen("Feature A: old", "Feature A: two", 1),
    );
    let third = export_change(
        &staging,
        &repo,
        "p3",
        &base,
        "notes.md",
        &notes.replacen("old", "three", 1),
    );
    let consumer = stage(&staging, &repo, "consumer");
    ThreeInputs {
        consumer,
        first,
        second,
        third,
        docs,
        notes,
    }
}

/// `fail` keeps the existing behavior: the conflicting input is reported and later inputs are
/// never attempted.
#[test]
fn fail_policy_stops_at_the_conflicting_input() {
    let tmp = TestDir::new("fail-policy");
    let inputs = three_inputs(&tmp);
    Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.first,
        "first",
        ConflictPolicy::Fail,
    )
    .unwrap();

    let error = Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.second,
        "second",
        ConflictPolicy::Fail,
    )
    .unwrap_err();
    match error {
        ApplyError::Conflict { paths, detail } => {
            assert_eq!(paths, ["docs.md"]);
            let detail = detail.unwrap_or_default();
            assert!(detail.contains("docs.md"), "{detail}");
            assert!(detail.contains("hunk at line"), "{detail}");
        }
        other => panic!("expected a conflict, got {other}"),
    }
    assert!(fs::read_to_string(inputs.consumer.path().join("docs.md"))
        .unwrap()
        .contains("Feature A: one"));
    assert_eq!(
        fs::read_to_string(inputs.consumer.path().join("notes.md")).unwrap(),
        inputs.notes
    );
}

/// `skip` leaves a bad patch out of the tree but keeps applying later inputs.
#[test]
fn skip_policy_reports_the_hunk_and_keeps_later_inputs() {
    let tmp = TestDir::new("skip-policy");
    let inputs = three_inputs(&tmp);
    Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.first,
        "first",
        ConflictPolicy::Skip,
    )
    .unwrap();

    let skipped = Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.second,
        "second",
        ConflictPolicy::Skip,
    )
    .unwrap();
    assert!(!skipped.applied);
    assert_eq!(skipped.conflicted_paths, ["docs.md"]);
    let detail = skipped.detail.unwrap_or_default();
    assert!(detail.contains("docs.md"), "{detail}");
    assert!(detail.contains("hunk at line"), "{detail}");

    let third = Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.third,
        "third",
        ConflictPolicy::Skip,
    )
    .unwrap();
    assert!(third.applied);
    let docs = fs::read_to_string(inputs.consumer.path().join("docs.md")).unwrap();
    assert!(docs.contains("Feature A: one"));
    assert!(!docs.contains("Feature A: two"));
    assert_eq!(
        docs,
        inputs.docs.replacen("Feature A: old", "Feature A: one", 1)
    );
    assert_eq!(
        fs::read_to_string(inputs.consumer.path().join("notes.md")).unwrap(),
        "Notes: three\n"
    );
}

/// `markers` keeps both sides, labels them with the input name, and still applies later inputs.
/// The marked-up file must also reach the exported `diff.patch`, while `.lu/` never does.
#[test]
fn markers_policy_exports_conflicts_and_keeps_later_inputs() {
    let tmp = TestDir::new("markers-policy");
    let inputs = three_inputs(&tmp);
    Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.first,
        "first",
        ConflictPolicy::Markers,
    )
    .unwrap();

    let conflicted = Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.second,
        "second",
        ConflictPolicy::Markers,
    )
    .unwrap();
    assert!(conflicted.applied);
    assert_eq!(conflicted.conflicted_paths, ["docs.md"]);
    let detail = conflicted.detail.unwrap_or_default();
    assert!(detail.contains("docs.md"), "{detail}");

    let docs = fs::read_to_string(inputs.consumer.path().join("docs.md")).unwrap();
    assert!(docs.contains("<<<<<<< HEAD\n"), "{docs}");
    assert!(docs.contains("Feature A: one\n"), "{docs}");
    assert!(docs.contains("=======\n"), "{docs}");
    assert!(docs.contains("Feature A: two\n"), "{docs}");
    assert!(docs.contains(">>>>>>> second\n"), "{docs}");

    let third = Staging::apply_input_diff(
        &inputs.consumer,
        &inputs.third,
        "third",
        ConflictPolicy::Markers,
    )
    .unwrap();
    assert!(third.applied);
    assert!(third.conflicted_paths.is_empty());
    assert_eq!(
        fs::read_to_string(inputs.consumer.path().join("notes.md")).unwrap(),
        "Notes: three\n"
    );

    let collected = prepare_inputs(&inputs.consumer).unwrap();
    let dir = create_input_dir(&collected, "second").unwrap();
    write_input_file(&dir, "report.json", b"{}").unwrap();
    let exported = String::from_utf8(Staging::export_diff(&inputs.consumer, 1 << 20).unwrap())
        .expect("diff must be UTF-8");
    assert!(exported.contains("+<<<<<<< HEAD\n"), "{exported}");
    assert!(exported.contains("+>>>>>>> second\n"), "{exported}");
    assert!(exported.contains("+Feature A: one\n"), "{exported}");
    assert!(exported.contains("+Feature A: two\n"), "{exported}");
    assert!(!exported.contains(INPUTS_DIR), "{exported}");
    assert!(!exported.contains("report.json"), "{exported}");
}
