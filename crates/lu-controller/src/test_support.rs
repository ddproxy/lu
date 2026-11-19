use std::path::Path;

use git2::{Repository, RepositoryInitOptions};

/// Creates a repository whose branch and commit settings are fixed for tests.
pub(crate) fn init_main_repo(path: &Path) -> Repository {
    let mut opts = RepositoryInitOptions::new();
    // The host's `init.defaultBranch` must not decide which ref the tests resolve.
    opts.initial_head("main");
    let repo = Repository::init_opts(path, &opts).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "lu-test").unwrap();
    config.set_str("user.email", "lu-test@example.net").unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    repo
}
