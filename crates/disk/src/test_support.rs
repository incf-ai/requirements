use std::path::{Path, PathBuf};

use syscalls::{ChangedPathsError, CommitAllError, CommitForPathError, Git};

/// A `Git` stub that reports a fixed commit for any path, used by tests that
/// exercise load/save plumbing in a scratch tempdir which isn't (and
/// shouldn't need to be) an actual git repository.
pub(crate) struct FixedGit;

impl Git for FixedGit {
    fn commit_for_path_excluding(
        &self,
        _path: &Path,
        _excludes: &[&Path],
    ) -> Result<String, CommitForPathError> {
        Ok("deadbeef".to_string())
    }

    fn changed_paths(&self, _dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
        Ok(Vec::new())
    }

    fn commit_all(&self, _dir: &Path, _message: &str) -> Result<(), CommitAllError> {
        Ok(())
    }
}

/// `git init`s a real, isolated scratch repo at `dir` (not this repository's
/// own history), with just user config — no commits. Used by tests that
/// need `syscalls::SystemGit` to see real, distinguishable commits (e.g.
/// verifying `include_attachments_in_commit`/`include_template_in_commit`
/// actually change which commit gets picked).
pub(crate) fn init_scratch_git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();

    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };

    run(&["init", "--quiet"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
}

/// Stages every change in `dir` and commits it, returning the new commit's
/// full hash.
pub(crate) fn git_commit_all(dir: &Path, message: &str) -> String {
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };

    run(&["add", "-A"]);
    run(&["commit", "--quiet", "-m", message]);

    String::from_utf8(
        std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string()
}
