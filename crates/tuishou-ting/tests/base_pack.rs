//! The agent can stage a controller-supplied pack with no `git` binary on PATH.

use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use git2::{Repository, Signature};
use lu_client::{ApiError, Controller, DependencyView, PollRequest};
use lu_config::{AgentConfig, BaseSource};
use lu_proto::{AgentEvent, ArtifactName, Completion, JobId, LogChunk};
use lu_staging::pack_commit;
use lu_supervise::CommandSpec;
use tokio::sync::oneshot;
use tuishou_ting::Agent;

struct PathGuard(Option<OsString>);

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }
}

struct PackController {
    spec: lu_proto::JobSpec,
    epoch: u64,
    commit: String,
    pack: Vec<u8>,
    assigned: AtomicBool,
    done: Arc<AtomicBool>,
}

impl Controller for PackController {
    fn deps<'a>(
        &'a self,
        _id: &'a lu_proto::JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DependencyView>, ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn dep_diff<'a>(
        &'a self,
        _id: &'a lu_proto::JobId,
        _dep: &'a lu_proto::JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ApiError>> + Send + 'a>> {
        Box::pin(async { Err(ApiError::NotFound) })
    }

    fn poll<'a>(
        &'a self,
        _request: &'a PollRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<AgentEvent>, ApiError>> + Send + 'a>> {
        Box::pin(async move {
            if self.assigned.swap(true, Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(Vec::new())
            } else {
                Ok(vec![AgentEvent::Assign {
                    job_id: JobId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
                    epoch: self.epoch,
                    spec: self.spec.clone(),
                    provider: None,
                    model: None,
                }])
            }
        })
    }

    fn job_base<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>, ApiError>> + Send + 'a>>
    {
        Box::pin(async move { Ok(Some((self.commit.clone(), self.pack.clone()))) })
    }

    fn set_staged_commit<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
        _commit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn merge_base<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
        commits: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<String, ApiError>> + Send + 'a>> {
        let commit = commits.first().cloned().unwrap_or_default();
        Box::pin(async move { Ok(commit) })
    }

    fn start<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn renew<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(false) })
    }

    fn append_log<'a>(
        &'a self,
        _chunk: &'a LogChunk,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn put_artifact<'a>(
        &'a self,
        _id: &'a JobId,
        _epoch: u64,
        _name: ArtifactName,
        _bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn complete<'a>(
        &'a self,
        _completion: &'a Completion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>> {
        let done = Arc::clone(&self.done);
        Box::pin(async move {
            done.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn staging_from_a_pack_needs_no_git_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("hello.txt"), "hello\n").unwrap();
    std::fs::write(source.join("nested/data.bin"), [0u8, 1, 2, 255]).unwrap();
    let mut opts = git2::RepositoryInitOptions::new();
    // Pin the branch so the pack's ref does not depend on the host's default branch name.
    opts.initial_head("main");
    let repo = Repository::init_opts(&source, &opts).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "tester").unwrap();
    config.set_str("user.email", "tester@example.net").unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    drop(config);
    let sig = Signature::now("tester", "tester@example.net").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("hello.txt")).unwrap();
    index
        .add_path(std::path::Path::new("nested/data.bin"))
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let oid = repo
        .commit(Some("refs/heads/main"), &sig, &sig, "base", &tree, &[])
        .unwrap();
    let commit = oid.to_string();
    let pack = pack_commit(&repo, oid).unwrap();
    drop(tree);
    drop(index);
    drop(repo);

    // A git stub outside PATH: if any stage step spawned `git`, the stub marker would appear.
    let stub_dir = tmp.path().join("stub-bin");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let stub_marker = tmp.path().join("git-stub-used");
    let stub = stub_dir.join("git");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho used > '{}'\nexit 1\n",
            stub_marker.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let harness_marker = tmp.path().join("harness-ran");
    let config = AgentConfig {
        name: "node-a".to_owned(),
        controller_url: "http://127.0.0.1:1".to_owned(),
        token_file: tmp.path().join("token").display().to_string(),
        ca_file: None,
        work_dir: work_dir.display().to_string(),
        harness: "opencode".to_owned(),
        base_source: BaseSource::Controller,
        opencode_server: None,
        workspace_group: None,
        harness_uid: None,
        harness_gid: None,
        allow_shared_uid_scope: false,
        opencode_password_file: None,
        harness_wrapper: Vec::new(),
        heartbeat_secs: 3,
        on_stuck: lu_config::StuckPolicy::None,
        stuck_after_secs: 1_800,
        reiki_health_url: None,
        reiki_token_file: None,
        provider_models: Vec::new(),
        labels: Vec::new(),
        taints: Vec::new(),
        label_files: Vec::new(),
        detect: Vec::new(),
        housekeeping: None,
        workspace: None,
        stack: None,
    };
    let done = Arc::new(AtomicBool::new(false));
    let spec = lu_proto::JobSpec {
        source: lu_proto::SourceSpec {
            repo: "https://example.invalid/source.git".to_owned(),
            git_ref: "main".to_owned(),
            base: Some(commit.clone()),
        },
        repo: "https://example.invalid/source.git".to_owned(),
        git_ref: "main".to_owned(),
        base_commit: Some(commit.clone()),
        task: "read the tree".to_owned(),
        harness: "opencode".to_owned(),
        provider: None,
        model: None,
        timeout_secs: 60,
        idle_timeout_secs: None,
        idempotency_key: None,
        after: Vec::new(),
        stack_union_paths: Vec::new(),
        stack_base_paths: Vec::new(),
        collect: lu_proto::CollectMode::Apply,
        include_failed: false,
        on_conflict: Default::default(),
        allow_empty: false,
        expect: None,
        requires: Vec::new(),
        on_stuck: None,
        stuck_after_secs: None,
        tolerates: Vec::new(),
        fallbacks: Vec::new(),
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
        schedule: None,
    };
    let controller = Arc::new(PackController {
        spec,
        epoch: 1,
        commit: commit.clone(),
        pack,
        assigned: AtomicBool::new(false),
        done: Arc::clone(&done),
    });
    let script = format!("echo ok > '{}'", harness_marker.display());
    let agent = Agent::new(config, controller)
        .unwrap()
        .with_command_builder(move |_task_file, _model, _server, _message| {
            Ok(CommandSpec::new("/bin/sh").arg("-c").arg(&script))
        });

    let _path_guard = PathGuard(std::env::var_os("PATH"));
    std::env::set_var("PATH", "");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the job never completed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = shutdown_tx.send(());
    handle.await.unwrap().unwrap();

    assert!(harness_marker.is_file(), "the harness did not run");
    assert!(!stub_marker.exists(), "a git stub was spawned");
    let _: PathBuf = stub_dir;
}
