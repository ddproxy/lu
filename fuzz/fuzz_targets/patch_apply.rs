#![no_main]
#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use lu_staging::{ConflictPolicy, Source, StageRequest, Staging};

struct Fixture {
    source: PathBuf,
    work: PathBuf,
    commit: String,
    pack: Vec<u8>,
    sequence: AtomicU64,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "lu-fuzz-patch-apply-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&root).expect("create fuzz fixture root");
        let source = root.join("source");
        let repo = git2::Repository::init(&source).expect("init source repository");
        fs::write(source.join("seed.txt"), b"seed\n").expect("write seed file");
        let mut index = repo.index().expect("open index");
        index
            .add_path(Path::new("seed.txt"))
            .expect("stage seed file");
        index.write().expect("write index");
        let tree = repo
            .find_tree(index.write_tree().expect("write tree"))
            .expect("read tree");
        let signature =
            git2::Signature::now("fuzz", "fuzz@example.invalid").expect("build signature");
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "seed", &tree, &[])
            .expect("create seed commit");
        let pack = lu_staging::pack_commit(&repo, commit).expect("pack seed commit");
        let work = root.join("work");
        fs::create_dir_all(&work).expect("create staging root");
        Fixture {
            source,
            work,
            commit: commit.to_string(),
            pack,
            sequence: AtomicU64::new(0),
        }
    })
}

// Applies arbitrary patch bytes to a real staged checkout. The worktree is recreated for each
// input and removed afterwards. Exported diffs must stay capped and must never contain a nested
// `.lu` path, even when the patch itself tried to create one.
fuzz_target!(|data: &[u8]| {
    let fixture = fixture();
    let id = fixture.sequence.fetch_add(1, Ordering::Relaxed);
    let job_id = format!("fuzz{id}");
    let request = StageRequest {
        job_id: job_id.clone(),
        source: Source::Path(fixture.source.clone()),
        git_ref: "main".to_owned(),
    };
    let staging = Staging::new(&fixture.work).expect("reuse fixture staging root");
    let staged = staging.stage_from_pack(request, &fixture.commit, &fixture.pack);
    let target = fixture.work.join(&job_id);
    if let Ok(staged) = staged {
        let result = Staging::apply_input_diff(&staged, data, "input", ConflictPolicy::Fail);
        if let Ok(applied) = &result {
            assert!(
                applied.conflicted_paths.len() <= data.len().saturating_add(1),
                "more conflict paths than patch bytes"
            );
            assert!(
                applied
                    .detail
                    .as_ref()
                    .is_none_or(|detail| detail.len() <= data.len().saturating_add(256)),
                "unbounded apply detail"
            );
        }
        let cap = 64 * 1024;
        match Staging::export_diff_with_paths(&staged, cap) {
            Ok(exported) => {
                assert!(exported.bytes.len() <= cap, "export exceeded its cap");
                for path in &exported.changed_paths {
                    assert!(
                        !lu_staging::is_lu_path(Path::new(path)),
                        "a .lu path escaped into the exported diff: {path}"
                    );
                }
            }
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.len() <= data.len().saturating_add(256),
                    "unbounded export error"
                );
            }
        }
        let _ = result;
    }
    let _ = fs::remove_dir_all(&target);
});
