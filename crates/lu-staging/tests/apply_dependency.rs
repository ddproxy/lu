//! Applying an exported `diff.patch` to a fresh checkout of the base commit.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lu_staging::{ApplyError, Source, StageRequest, StagedDir, Staging};

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
    let repo = tmp.0.join("source");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "--local", "user.email", "t@e.net"]);
    git(&repo, &["config", "--local", "user.name", "T"]);
    fs::write(repo.join("docs.md"), content).unwrap();
    git(&repo, &["add", "docs.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
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
