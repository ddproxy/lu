//! End-to-end publish tests against a local bare repository.
//!
//! The controller never sees a real forge here: `[publish.remotes.*].test_file_url` redirects the
//! push to a temporary bare repo while the configured URL stays a non-credential `shim.invalid`
//! HTTPS URL. Every push still goes through the same git2 code path, CAS check and refspec rules as
//! production.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use git2::build::CheckoutBuilder;
use git2::{DiffFormat, Oid, Repository, Signature};
use lu_audit::Writer;
use lu_config::{
    ControllerAgent, ControllerConfig, ControllerOperator, ControllerRepo, PublishConfig,
    PublishGate, PublishRemote,
};
use lu_controller::publish::{self, PublishError};
use lu_controller::time::now;
use lu_controller::AppState;
use lu_proto::{
    GateFailureMode, JobId, JobSpec, PublishSpec, PublishTarget, DEFAULT_PUBLISH_BRANCH,
};
use lu_store::sqlite::SqliteStore;
use lu_store::Store;
use serde_json::Value;
use tokio::task::JoinHandle;

const TOKEN: &str = "ghp_SeCrEt9f3aC0rnerstone";
const SOURCE_BRANCH: &str = "feature/x";
const KEY: &str = "case-1";

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    artifacts: PathBuf,
    source: PathBuf,
    mirror: PathBuf,
    remote: PathBuf,
    token_file: PathBuf,
    audit_file: PathBuf,
    source_url: String,
    base: Oid,
    desired: Oid,
    patch: Vec<u8>,
    state: Arc<AppState>,
    store: Arc<SqliteStore>,
    _audit_task: JoinHandle<()>,
}

impl Fixture {
    async fn new(edit: impl FnOnce(&mut PublishConfig, &Path)) -> Self {
        Self::with_source("https://shim.invalid/org/target.git", edit).await
    }

    async fn with_source(source_url: &str, edit: impl FnOnce(&mut PublishConfig, &Path)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let artifacts = root.join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let mirror = root.join("mirrors").join("source.git");
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        Repository::init_bare(&mirror).unwrap();
        let remote = root.join("publish.git");
        Repository::init_bare(&remote).unwrap();

        let source = root.join("source");
        let (base, desired, patch) = build_source(&source, &mirror);
        // A source-branch push compares against the branch as it was when the job was pinned.
        push_refspec(
            &source,
            &remote,
            &format!("{base}:refs/heads/{SOURCE_BRANCH}"),
        );

        let token_file = root.join("token");
        std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let db = root.join("lu.db");
        let cfg = controller_config(source_url, &db, &artifacts);
        let store = Arc::new(SqliteStore::open(&db).unwrap());
        let state = AppState::from_config(&cfg, store.clone()).unwrap();
        let (writer, audit_task) = Writer::open(
            &root.join("audit.jsonl"),
            Arc::clone(&state.audit_write_errors),
        )
        .await
        .unwrap();
        state.set_audit(writer);
        let mut publish_cfg = publish_config(&token_file, &remote);
        edit(&mut publish_cfg, &remote);
        state.set_publish(publish_cfg);

        let audit_file = root.join("audit.jsonl");
        Self {
            _tmp: tmp,
            root,
            artifacts,
            source,
            mirror,
            remote,
            token_file,
            audit_file,
            source_url: source_url.to_owned(),
            base,
            desired,
            patch,
            state: Arc::new(state),
            store,
            _audit_task: audit_task,
        }
    }

    fn job(&self, edit: impl FnOnce(&mut JobSpec)) -> JobRecord {
        let mut spec = sample_spec(&self.source_url);
        spec.git_ref = SOURCE_BRANCH.to_owned();
        spec.source.git_ref = SOURCE_BRANCH.to_owned();
        spec.base_commit = Some(self.base.to_string());
        spec.idempotency_key = Some(KEY.to_owned());
        spec.publish = Some(source_branch_spec());
        edit(&mut spec);
        let (job, _) = self.store.create_job(&spec, now()).unwrap();
        let dir = self.artifacts.join(job.id.as_str());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("diff.patch"), &self.patch).unwrap();
        job
    }

    async fn publish(&self, job: &JobRecord) -> Result<lu_proto::PublishRecord, PublishError> {
        publish::publish(Arc::clone(&self.state), job.clone()).await
    }

    /// Pushes one ref into a bare repo from the fixture's source repo.
    fn push(&self, target: &Path, refspec: &str) {
        push_refspec(&self.source, target, refspec);
    }

    fn remote_refs(&self) -> BTreeMap<String, Oid> {
        refs(&self.remote)
    }

    fn metric(&self, target: &str, result: &str) -> u64 {
        self.state
            .publish_totals()
            .into_iter()
            .find(|(t, r, _)| t == target && r == result)
            .map(|(_, _, n)| n)
            .unwrap_or(0)
    }

    async fn wait_for_stored(&self, id: &JobId, detail: &str) -> lu_proto::PublishRecord {
        for _ in 0..100 {
            let job = self.store.get_job(id).unwrap();
            if let Some(record) = job.publish {
                if record.detail == detail {
                    return record;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no stored publish record with detail {detail:?}");
    }

    async fn wait_for_audit(&self, target: &str, result: &str) -> Value {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&self.audit_file) {
                for line in text.lines() {
                    let value: Value = serde_json::from_str(line).unwrap();
                    if value["event"] == "publish"
                        && value["data"]["target"] == target
                        && value["data"]["result"] == result
                    {
                        return value;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no publish audit event for {target}/{result}");
    }

    /// The push token must never appear anywhere except its owner-only source file.
    fn assert_token_is_private(&self) {
        fn walk(path: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut paths = Vec::new();
        walk(&self.root, &mut paths);
        for path in paths {
            if path == self.token_file {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap_or_default();
            assert!(
                !windows(&bytes, TOKEN.as_bytes()),
                "push token leaked into {}",
                path.display()
            );
        }
    }
}

type JobRecord = lu_store::JobRecord;

fn controller_config(source_url: &str, db: &Path, artifacts: &Path) -> ControllerConfig {
    ControllerConfig {
        listen: "127.0.0.1:0".to_owned(),
        db: db.to_string_lossy().into_owned(),
        artifacts_dir: artifacts.to_string_lossy().into_owned(),
        lease_secs: 6,
        max_retries: 1,
        node_attempts: 2,
        provider_cooldown_secs: 60,
        recovery_grace_secs: 1,
        max_connections: 16,
        max_connections_per_addr: 8,
        local_repo_roots: Vec::new(),
        agents: Vec::<ControllerAgent>::new(),
        operators: Vec::<ControllerOperator>::new(),
        repos: vec![ControllerRepo {
            url: source_url.to_owned(),
            name: "source".to_owned(),
            credential_file: None,
            refs: vec!["*".to_owned()],
            allow_file_urls: false,
        }],
        groups: Vec::new(),
        blocked_dependent_ttl: "24h".to_owned(),
        read_per_minute: lu_config::DEFAULT_READ_PER_MINUTE,
    }
}

fn publish_config(token_file: &Path, remote: &Path) -> PublishConfig {
    let remote = PublishRemote {
        url: "https://shim.invalid/org/target.git".to_owned(),
        token_file: token_file.to_string_lossy().into_owned(),
        branch_prefixes: vec!["snapshot/".to_owned()],
        allow_source_branch_push: false,
        max_pushes_per_hour: 100,
        test_file_url: Some(format!("file://{}", remote.display())),
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
    config.remotes.insert("forge".to_owned(), remote);
    config
}

fn add_second_remote(config: &mut PublishConfig, _remote: &Path) {
    let mut other = config.remotes["forge"].clone();
    other.url = "https://shim.invalid/org/other.git".to_owned();
    config.remotes.insert("other".to_owned(), other);
}

fn source_branch_spec() -> PublishSpec {
    PublishSpec {
        branch: None,
        gates: Vec::new(),
        on_gate_failure: GateFailureMode::Hold,
        remote: None,
        target: PublishTarget::SourceBranch,
        allow_source_branch: true,
    }
}

fn sample_spec(source_url: &str) -> JobSpec {
    JobSpec {
        source: lu_proto::SourceSpec {
            repo: source_url.to_owned(),
            git_ref: SOURCE_BRANCH.to_owned(),
            base: None,
        },
        repo: source_url.to_owned(),
        git_ref: SOURCE_BRANCH.to_owned(),
        base_commit: None,
        task: "publish this job".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 3600,
        idle_timeout_secs: None,
        idempotency_key: None,
        after: Vec::new(),
        collect: lu_proto::CollectMode::Apply,
        include_failed: false,
        on_conflict: Default::default(),
        allow_empty: false,
        requires: Vec::new(),
        tolerates: Vec::new(),
        on_stuck: None,
        stuck_after_secs: None,
        priority: lu_proto::DEFAULT_PRIORITY,
        group: None,
        allow_spawn: false,
        spawn_inherit: false,
        spawn_repos: Vec::new(),
        workspace: None,
        cache_dirs: Vec::new(),
        context: None,
        origin: None,
        origin_ref: None,
        untrusted_context: false,
        draft: false,
        publish: None,
        fallbacks: Vec::new(),
        expect: None,
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
        schedule: None,
    }
}

fn build_source(path: &Path, mirror: &Path) -> (Oid, Oid, Vec<u8>) {
    let repo = Repository::init(path).unwrap();
    let base = commit_file(&repo, "base", "base\n", &[]);
    let base_commit = repo.find_commit(base).unwrap();
    repo.branch(SOURCE_BRANCH, &base_commit, false).unwrap();

    repo.set_head_detached(base).unwrap();
    repo.checkout_tree(
        base_commit.as_object(),
        Some(CheckoutBuilder::new().force()),
    )
    .unwrap();
    let desired = commit_file(&repo, "desired", "published\n", &[base]);

    repo.set_head(&format!("refs/heads/{SOURCE_BRANCH}"))
        .unwrap();
    repo.checkout_head(Some(CheckoutBuilder::new().force()))
        .unwrap();

    let patch = patch_between(&repo, base, desired);

    let url = mirror.to_string_lossy();
    let mut remote = repo.remote("mirror", &url).unwrap();
    let refs = [
        format!("refs/heads/{SOURCE_BRANCH}:refs/heads/{SOURCE_BRANCH}"),
        "refs/heads/feature/x:refs/lu-pr/7/head".to_owned(),
        format!("{desired}:refs/lu-pr/8/head"),
    ];
    let specs: Vec<&str> = refs.iter().map(String::as_str).collect();
    remote.push(&specs, None).unwrap();
    Repository::open(mirror)
        .unwrap()
        .set_head(&format!("refs/heads/{SOURCE_BRANCH}"))
        .unwrap();
    (base, desired, patch)
}

fn push_refspec(source: &Path, target: &Path, refspec: &str) {
    let repo = Repository::open(source).unwrap();
    let url = target.to_string_lossy();
    if repo.find_remote("fixture-push").is_ok() {
        repo.remote_delete("fixture-push").unwrap();
    }
    let mut remote = repo.remote("fixture-push", &url).unwrap();
    remote.push(&[refspec], None).unwrap();
}

fn commit_file(repo: &Repository, message: &str, content: &str, parents: &[Oid]) -> Oid {
    std::fs::write(repo.workdir().unwrap().join("README.md"), content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = Signature::now("Lu Test", "test@example.invalid").unwrap();
    let parent_commits: Vec<_> = parents
        .iter()
        .map(|oid| repo.find_commit(*oid).unwrap())
        .collect();
    let parent_refs: Vec<&git2::Commit<'_>> = parent_commits.iter().collect();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parent_refs,
    )
    .unwrap()
}

fn patch_between(repo: &Repository, old: Oid, new: Oid) -> Vec<u8> {
    let old_tree = repo.find_commit(old).unwrap().tree().unwrap();
    let new_tree = repo.find_commit(new).unwrap().tree().unwrap();
    let diff = repo
        .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
        .unwrap();
    let mut out = Vec::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        // libgit2's print callback omits the unified-diff prefix on body lines.
        let origin = line.origin();
        if matches!(origin, ' ' | '+' | '-') {
            out.push(origin as u8);
        }
        out.extend_from_slice(line.content());
        true
    })
    .unwrap();
    out
}

fn refs(path: &Path) -> BTreeMap<String, Oid> {
    let repo = Repository::open(path).unwrap();
    repo.references()
        .unwrap()
        .flatten()
        .filter_map(|reference| {
            let name = reference.name().ok()?.to_owned();
            reference.target().map(|oid| (name, oid))
        })
        .collect()
}

fn windows(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

async fn stored_record(fixture: &Fixture, id: &JobId) -> lu_proto::PublishRecord {
    fixture.wait_for_stored(id, "published").await
}

#[tokio::test]
async fn source_branch_push_updates_one_ref_and_records_audit_and_metrics() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .branch_prefixes
            .push("feature/".to_owned());
    })
    .await;
    fixture.push(
        &fixture.remote,
        &format!("{}:refs/heads/unrelated", fixture.base),
    );
    fixture.push(&fixture.remote, &format!("{}:refs/tags/v1", fixture.base));
    let before = fixture.remote_refs();

    let job = fixture.job(|_| {});
    let view_text = publish::target_text(&fixture.state, &job);
    assert_eq!(
        view_text.as_deref(),
        Some("forge:feature/x (source branch)")
    );

    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert_eq!(record.target, PublishTarget::SourceBranch);
    assert_eq!(record.source_branch.as_deref(), Some(SOURCE_BRANCH));
    assert_eq!(record.branch, SOURCE_BRANCH);
    assert!(record.commit.is_some());
    assert!(!format!("{record:?}").contains(TOKEN));
    assert!(!view_text.unwrap().contains(TOKEN));

    let after = fixture.remote_refs();
    let expected = format!("refs/heads/{SOURCE_BRANCH}");
    assert_eq!(after.len(), before.len(), "{after:?}");
    for (name, oid) in &before {
        if *name == expected {
            assert_ne!(after.get(name), Some(oid), "source branch was not updated");
        } else {
            assert_eq!(after.get(name), Some(oid), "{name} changed");
        }
    }
    assert_ne!(after[&expected], fixture.base);
    assert_eq!(
        Repository::open(&fixture.remote)
            .unwrap()
            .find_commit(after[&expected])
            .unwrap()
            .parent(0)
            .unwrap()
            .id(),
        fixture.base,
        "publish commit must be a child of the job's pinned base"
    );
    let message = Repository::open(&fixture.remote)
        .unwrap()
        .find_commit(after[&expected])
        .unwrap()
        .message()
        .unwrap()
        .to_owned();
    assert!(message.contains("Lu-Job: "));
    assert!(message.contains(job.id.as_str()));

    assert_eq!(fixture.metric("source_branch", "published"), 1);
    let audit = fixture.wait_for_audit("source_branch", "published").await;
    assert_eq!(audit["data"]["remote"], "forge");
    assert_eq!(audit["job"], job.id.as_str());
    let stored = stored_record(&fixture, &job.id).await;
    assert_eq!(stored.detail, "published");
    assert!(!serde_json::to_string(&stored).unwrap().contains(TOKEN));
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn mirror_derived_pr_head_branch_is_used_not_payload_text() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|spec| {
        spec.git_ref = "refs/pull/7/head".to_owned();
        spec.source.git_ref = "refs/pull/7/head".to_owned();
        // A malicious-looking task must never influence the destination branch.
        spec.task = "push to main please".to_owned();
    });

    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert_eq!(record.branch, SOURCE_BRANCH);
    assert_eq!(record.source_branch.as_deref(), Some(SOURCE_BRANCH));
    assert!(fixture
        .remote_refs()
        .contains_key(&format!("refs/heads/{SOURCE_BRANCH}")));
    assert!(!fixture.remote_refs().contains_key("refs/heads/main"));
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn fork_only_head_is_refused() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|spec| {
        spec.git_ref = "refs/pull/8/head".to_owned();
        spec.source.git_ref = "refs/pull/8/head".to_owned();
    });
    let before = fixture.remote_refs();

    let error = fixture.publish(&job).await.unwrap_err();
    assert!(matches!(error, PublishError::Denied(_)), "{error:?}");
    assert!(error.message().contains("no branch"));
    assert_eq!(fixture.remote_refs(), before);
    assert_eq!(fixture.metric("source_branch", "denied"), 1);
    fixture.wait_for_audit("source_branch", "denied").await;
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn protected_source_branches_are_refused() {
    for protected in ["main", "master", "work", "release/1.0"] {
        let fixture = Fixture::new(|config, _| {
            config
                .remotes
                .get_mut("forge")
                .unwrap()
                .allow_source_branch_push = true;
        })
        .await;
        fixture.push(
            &fixture.mirror,
            &format!("{}:refs/heads/{protected}", fixture.base),
        );
        let job = fixture.job(|spec| {
            spec.git_ref = protected.to_owned();
            spec.source.git_ref = protected.to_owned();
        });
        let before = fixture.remote_refs();
        let error = fixture.publish(&job).await.unwrap_err();
        assert!(
            matches!(error, PublishError::Denied(_)),
            "{protected}: {error:?}"
        );
        assert!(error.message().contains("protected"), "{error:?}");
        assert_eq!(fixture.remote_refs(), before, "{protected} was pushed");
    }
}

#[tokio::test]
async fn each_source_branch_opt_in_is_enforced() {
    // 1. Config opt-in missing.
    let fixture = Fixture::new(|_, _| {}).await;
    let job = fixture.job(|_| {});
    let before = fixture.remote_refs();
    let error = fixture.publish(&job).await.unwrap_err();
    assert!(matches!(error, PublishError::Denied(_)));
    assert!(error.message().contains("does not allow"), "{error:?}");
    assert_eq!(fixture.remote_refs(), before);
    assert_eq!(fixture.metric("source_branch", "denied"), 1);

    // 2. Spec allowance missing.
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|spec| spec.publish.as_mut().unwrap().allow_source_branch = false);
    let before = fixture.remote_refs();
    let error = fixture.publish(&job).await.unwrap_err();
    assert!(matches!(error, PublishError::Denied(_)));
    assert!(
        error.message().contains("origin did not allow"),
        "{error:?}"
    );
    assert_eq!(fixture.remote_refs(), before);

    // 3. Origin permission missing: an unapproved intake draft cannot push.
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|spec| {
        spec.origin = Some("github:webhook".to_owned());
        spec.draft = true;
    });
    let before = fixture.remote_refs();
    let error = fixture.publish(&job).await.unwrap_err();
    assert!(matches!(error, PublishError::Denied(_)));
    assert!(
        error
            .message()
            .contains("operator submission or draft approval"),
        "{error:?}"
    );
    assert_eq!(fixture.remote_refs(), before);

    // 4. Spec target missing: the same job defaults to a snapshot, never the source branch.
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|spec| {
        spec.publish.as_mut().unwrap().target = PublishTarget::Snapshot;
    });
    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert_eq!(record.target, PublishTarget::Snapshot);
    assert_eq!(record.branch, DEFAULT_PUBLISH_BRANCH.replace("<key>", KEY));
    assert_eq!(
        fixture.remote_refs()[&format!("refs/heads/{SOURCE_BRANCH}")],
        fixture.base,
        "a snapshot target must not update the source branch"
    );
    assert!(fixture
        .remote_refs()
        .contains_key(&format!("refs/heads/{}", record.branch)));
    assert_eq!(fixture.metric("source_branch", "published"), 0);
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn approved_draft_is_allowed() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|spec| {
        spec.origin = Some("github:webhook".to_owned());
        spec.draft = true;
    });
    fixture.state.mark_approved(&job.id);
    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert_eq!(record.branch, SOURCE_BRANCH);
}

#[tokio::test]
async fn explicit_remote_wins_over_redirect_and_unmatched_sources_default_deny() {
    let fixture =
        Fixture::with_source("https://stranger.invalid/org/repo.git", |config, remote| {
            add_second_remote(config, remote);
            config
                .redirects
                .insert("stranger.invalid/org/repo".to_owned(), "other".to_owned());
            config
                .remotes
                .get_mut("forge")
                .unwrap()
                .allow_source_branch_push = true;
        })
        .await;
    let job = fixture.job(|spec| {
        spec.publish.as_mut().unwrap().remote = Some("forge".to_owned());
    });
    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert_eq!(record.remote, "forge");

    let denied = Fixture::with_source("https://stranger.invalid/org/repo.git", |_, _| {}).await;
    let job = denied.job(|_| {});
    let before = denied.remote_refs();
    let error = denied.publish(&job).await.unwrap_err();
    assert!(matches!(error, PublishError::Denied(_)));
    assert!(error.message().contains("no redirect"), "{error:?}");
    assert_eq!(denied.remote_refs(), before);
    denied.assert_token_is_private();
}

#[tokio::test]
async fn redirect_supplies_the_default_target() {
    let fixture = Fixture::with_source("https://stranger.invalid/org/repo.git", |config, _| {
        config
            .redirects
            .insert("stranger.invalid/org/repo".to_owned(), "forge".to_owned());
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|_| {});
    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert_eq!(record.remote, "forge");
    assert_eq!(record.branch, SOURCE_BRANCH);
}

#[tokio::test]
async fn cas_miss_falls_back_to_a_snapshot_branch() {
    let fixture = Fixture::new(|config, remote_path| {
        let remote = config.remotes.get_mut("forge").unwrap();
        remote.allow_source_branch_push = true;
        remote.branch_prefixes.push("feature/".to_owned());
        let _ = remote_path;
    })
    .await;
    // Move the source branch after the job was pinned to `base`.
    fixture.push(
        &fixture.remote,
        &format!("{}:refs/heads/{SOURCE_BRANCH}", fixture.desired),
    );
    let moved = fixture.remote_refs()[&format!("refs/heads/{SOURCE_BRANCH}")];
    let job = fixture.job(|_| {});

    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published_snapshot_fallback");
    assert_eq!(record.target, PublishTarget::SourceBranch);
    assert_eq!(record.branch, DEFAULT_PUBLISH_BRANCH.replace("<key>", KEY));
    assert_eq!(record.source_branch.as_deref(), Some(SOURCE_BRANCH));
    assert_eq!(
        fixture.remote_refs()[&format!("refs/heads/{SOURCE_BRANCH}")],
        moved,
        "source branch must not be touched after a CAS miss"
    );
    assert!(fixture
        .remote_refs()
        .contains_key(&format!("refs/heads/{}", record.branch)));
    assert_eq!(
        fixture.metric("source_branch", "published_snapshot_fallback"),
        1
    );
    fixture
        .wait_for_audit("source_branch", "published_snapshot_fallback")
        .await;
    let stored = fixture
        .wait_for_stored(&job.id, "published_snapshot_fallback")
        .await;
    assert_eq!(stored.branch, record.branch);
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn cas_miss_without_a_snapshot_prefix_fails_without_pushing() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
        config.remotes.get_mut("forge").unwrap().branch_prefixes = vec!["review/".to_owned()];
    })
    .await;
    fixture.push(
        &fixture.remote,
        &format!("{}:refs/heads/{SOURCE_BRANCH}", fixture.desired),
    );
    let moved = fixture.remote_refs()[&format!("refs/heads/{SOURCE_BRANCH}")];
    let job = fixture.job(|_| {});

    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "no_snapshot_prefix");
    assert_eq!(
        fixture.remote_refs()[&format!("refs/heads/{SOURCE_BRANCH}")],
        moved
    );
    assert_eq!(fixture.remote_refs().len(), 1);
    assert_eq!(fixture.metric("source_branch", "no_snapshot_prefix"), 1);
    fixture
        .wait_for_audit("source_branch", "no_snapshot_prefix")
        .await;
}

#[tokio::test]
async fn non_fast_forward_push_is_rejected_without_force() {
    let fixture = Fixture::new(|_, _| {}).await;
    // A sibling commit on the snapshot branch cannot be replaced by a non-force push.
    fixture.push(
        &fixture.remote,
        &format!(
            "{}:refs/heads/{}",
            fixture.desired,
            DEFAULT_PUBLISH_BRANCH.replace("<key>", KEY)
        ),
    );
    let snapshot = format!(
        "refs/heads/{}",
        DEFAULT_PUBLISH_BRANCH.replace("<key>", KEY)
    );
    let original = fixture.remote_refs()[&snapshot];
    let job = fixture.job(|spec| {
        spec.publish.as_mut().unwrap().target = PublishTarget::Snapshot;
    });

    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "failed");
    assert_eq!(
        fixture.remote_refs()[&snapshot],
        original,
        "remote was force-updated"
    );
    assert_eq!(fixture.remote_refs().len(), 2, "an extra ref was pushed");
    assert_eq!(fixture.metric("snapshot", "failed"), 1);
    fixture.wait_for_audit("snapshot", "failed").await;
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn a_failed_gate_blocks_source_branch_even_with_push_labelled() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
        config.gates.insert(
            "check".to_owned(),
            PublishGate {
                command: "false".to_owned(),
                timeout_secs: 5,
                expected_exit_code: 0,
            },
        );
    })
    .await;
    let job = fixture.job(|spec| {
        spec.publish.as_mut().unwrap().gates = vec!["check".to_owned()];
        spec.publish.as_mut().unwrap().on_gate_failure = GateFailureMode::PushLabelled;
    });
    let before = fixture.remote_refs();

    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "gates_failed");
    assert!(record.gates.iter().any(|gate| gate.result == "fail"));
    assert_eq!(fixture.remote_refs(), before);
    assert_eq!(fixture.metric("source_branch", "gates_failed"), 1);
    fixture
        .wait_for_audit("source_branch", "gates_failed")
        .await;
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn passing_gates_run_before_the_push() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
        config.gates.insert(
            "check".to_owned(),
            PublishGate {
                command: "true".to_owned(),
                timeout_secs: 5,
                expected_exit_code: 0,
            },
        );
    })
    .await;
    let job = fixture.job(|spec| {
        spec.publish.as_mut().unwrap().gates = vec!["check".to_owned()];
    });
    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert!(record.gates.iter().all(|gate| gate.result == "pass"));
    assert!(fixture
        .remote_refs()
        .contains_key(&format!("refs/heads/{SOURCE_BRANCH}")));
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn source_remote_that_is_a_publish_remote_needs_no_redirect() {
    let url = "https://shim.invalid/org/target.git";
    let fixture = Fixture::with_source(url, |config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let job = fixture.job(|_| {});
    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "published");
    assert_eq!(record.remote, "forge");
    assert_eq!(fixture.metric("source_branch", "published"), 1);
}

#[tokio::test]
async fn max_pushes_per_hour_limits_successful_pushes() {
    let fixture = Fixture::new(|config, _| {
        config.remotes.get_mut("forge").unwrap().max_pushes_per_hour = 1;
    })
    .await;
    let first = fixture.job(|spec| {
        spec.idempotency_key = Some("one".to_owned());
        spec.publish.as_mut().unwrap().target = PublishTarget::Snapshot;
    });
    assert_eq!(fixture.publish(&first).await.unwrap().detail, "published");

    let second = fixture.job(|spec| {
        spec.idempotency_key = Some("two".to_owned());
        spec.publish.as_mut().unwrap().target = PublishTarget::Snapshot;
    });
    let error = fixture.publish(&second).await.unwrap_err();
    assert!(matches!(error, PublishError::RateLimited(_)), "{error:?}");
    assert!(!fixture
        .remote_refs()
        .contains_key("refs/heads/snapshot/two"));
    assert_eq!(fixture.metric("snapshot", "rate_limited"), 1);
    fixture.wait_for_audit("snapshot", "rate_limited").await;
    fixture.assert_token_is_private();
}

#[tokio::test]
async fn publish_refuses_patch_with_secret_hits_unless_allow_scrubbed() {
    let fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
    })
    .await;
    let mut job = fixture.job(|_| {});
    job.patch_secret_hits = lu_proto::view::PatchSecretHitsView {
        count: 1,
        rules: vec!["private_key".to_owned()],
    };

    let record = fixture.publish(&job).await.unwrap();
    assert_eq!(record.detail, "failed", "{record:?}");
    assert_eq!(fixture.metric("source_branch", "failed"), 1);

    let allowed_fixture = Fixture::new(|config, _| {
        config
            .remotes
            .get_mut("forge")
            .unwrap()
            .allow_source_branch_push = true;
        config.allow_scrubbed = true;
    })
    .await;
    let mut allowed = allowed_fixture.job(|spec| {
        spec.idempotency_key = Some("psh-allowed".to_owned());
    });
    allowed.patch_secret_hits = lu_proto::view::PatchSecretHitsView {
        count: 1,
        rules: vec!["private_key".to_owned()],
    };
    let record = allowed_fixture.publish(&allowed).await.unwrap();
    assert_eq!(record.detail, "published", "{record:?}");
}
