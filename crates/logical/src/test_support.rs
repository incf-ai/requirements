use std::path::Path;

use syscalls::{CommitForPathError, CommitForRemoteError, Git, RemoteGit};

/// A `Git` stub that reports a fixed commit for any path — mirrors
/// `disk`'s own `test_support::FixedGit`, used here for the same reason:
/// tests that load the sample project (a real git repo) shouldn't depend
/// on its actual commit history.
pub(crate) struct FixedGit;

impl Git for FixedGit {
    fn commit_for_path_excluding(
        &self,
        _path: &Path,
        _excludes: &[&Path],
    ) -> Result<String, CommitForPathError> {
        Ok("deadbeef".to_string())
    }
}

/// A `RemoteGit` stub that reports a fixed commit for any remote
/// URL/path, used by validation tests that resolve `RemoteReferenceV1`
/// dependencies without needing real network access.
pub(crate) struct FixedRemoteGit;

impl RemoteGit for FixedRemoteGit {
    fn commit_for_remote(
        &self,
        _url: &str,
        _path: Option<&Path>,
    ) -> Result<String, CommitForRemoteError> {
        Ok("deadbeef".to_string())
    }
}
