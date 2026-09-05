use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use thiserror::Error;

pub trait Filesystem {
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn is_dir(&self, path: &Path) -> bool;
    fn exists(&self, path: &Path) -> bool;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StdFilesystem;

impl Filesystem for StdFilesystem {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        std::fs::write(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }
}

/// Wraps another `Filesystem`, letting tests force specific calls on
/// specific paths to fail instead of touching real disk state.
#[derive(Debug, Default)]
pub struct FaultInjectingFilesystem<F> {
    inner: F,
    faults: HashMap<PathBuf, io::ErrorKind>,
}

impl<F: Filesystem> FaultInjectingFilesystem<F> {
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            faults: HashMap::new(),
        }
    }

    /// Every call touching `path` will fail with `kind` until removed.
    pub fn inject(&mut self, path: impl Into<PathBuf>, kind: io::ErrorKind) {
        self.faults.insert(path.into(), kind);
    }

    pub fn clear(&mut self, path: &Path) {
        self.faults.remove(path);
    }

    fn fault(&self, path: &Path) -> Option<io::Error> {
        self.faults
            .get(path)
            .map(|kind| io::Error::new(*kind, format!("injected fault for {}", path.display())))
    }
}

impl<F: Filesystem> Filesystem for FaultInjectingFilesystem<F> {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        if let Some(err) = self.fault(path) {
            return Err(err);
        }
        self.inner.read_to_string(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if let Some(err) = self.fault(path) {
            return Err(err);
        }
        self.inner.read(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        if let Some(err) = self.fault(path) {
            return Err(err);
        }
        self.inner.read_dir(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.inner.is_dir(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        if let Some(err) = self.fault(path) {
            return Err(err);
        }
        self.inner.write(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        if let Some(err) = self.fault(path) {
            return Err(err);
        }
        self.inner.create_dir_all(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        if let Some(err) = self.fault(path) {
            return Err(err);
        }
        self.inner.remove_dir_all(path)
    }
}

/// Wraps another `Filesystem`, sleeping `delay` before every *mutating*
/// call (`write`/`create_dir_all`/`remove_dir_all`) — letting tests make a real save
/// artificially slow instead of racing however fast the real filesystem
/// happens to be, without also slowing down reads (so loading a project
/// through this wrapper stays fast; only saving one is affected). Real
/// filesystem operations against a small project are often fast enough
/// (sub-millisecond, no network or subprocess involved, unlike `Git`) to
/// win a race against a deliberately short test timeout, which otherwise
/// makes a "still in progress" state impossible to observe reliably —
/// see `gui-ui`'s exit-dialog Saving/TimedOut tests.
#[derive(Debug, Clone, Copy)]
pub struct SlowFilesystem<F> {
    inner: F,
    delay: Duration,
}

impl<F: Filesystem> SlowFilesystem<F> {
    pub fn new(inner: F, delay: Duration) -> Self {
        Self { inner, delay }
    }
}

impl<F: Filesystem> Filesystem for SlowFilesystem<F> {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.inner.read_to_string(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.read_dir(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.inner.is_dir(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        std::thread::sleep(self.delay);
        self.inner.write(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::thread::sleep(self.delay);
        self.inner.create_dir_all(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::thread::sleep(self.delay);
        self.inner.remove_dir_all(path)
    }
}

pub trait Git {
    /// The newest commit touching anything under `path`, ignoring any
    /// changes confined entirely to paths in `excludes` (each typically a
    /// subdirectory of `path`, e.g. `path.join("attachments")`).
    fn commit_for_path_excluding(
        &self,
        path: &Path,
        excludes: &[&Path],
    ) -> Result<String, CommitForPathError>;

    fn commit_for_path(&self, path: &Path) -> Result<String, CommitForPathError> {
        self.commit_for_path_excluding(path, &[])
    }

    /// Whether `dir` is inside a git working tree at all. Every other
    /// method on this trait assumes it is (a project's commit lookups
    /// fail, confusingly, one leaf at a time otherwise) — callers that can
    /// give a clear up-front error should check this first. Defaults to
    /// `true` so the fakes used across the workspace's tests, which never
    /// touch a real `.git`, keep behaving as if everything's a repository
    /// without each needing its own override.
    fn is_repository(&self, _dir: &Path) -> bool {
        true
    }

    /// `git init`s `dir` in place. Defaults to a no-op for the same reason
    /// as `is_repository`'s default — only `SystemGit` needs this for
    /// real.
    fn init_repository(&self, _dir: &Path) -> Result<(), InitRepositoryError> {
        Ok(())
    }

    /// Every path under `dir` that a `commit_all` would sweep up right
    /// now — staged, unstaged, and untracked — as paths relative to
    /// `dir` (so callers get something directly displayable/sortable
    /// without stripping a repo-root prefix themselves). Order is
    /// whatever `git status` happens to emit; callers that want a
    /// specific order (e.g. shallow-paths-first) sort it themselves.
    fn changed_paths(&self, dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError>;

    /// Stages every change under `dir` (`git add -A`) and commits it with
    /// `message` (`git commit -m`). Fails as `NothingToCommit` rather than
    /// letting `git commit` itself error, so callers can distinguish "the
    /// repo really has nothing pending" from a genuine git failure.
    fn commit_all(&self, dir: &Path, message: &str) -> Result<(), CommitAllError>;
}

#[derive(Debug, Error)]
pub enum CommitForPathError {
    #[error("failed to run git: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
    #[error("no commit touches {path}")]
    NotTracked { path: PathBuf },
}

#[derive(Debug, Error)]
pub enum InitRepositoryError {
    #[error("failed to run git init: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git init exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
}

#[derive(Debug, Error)]
pub enum ChangedPathsError {
    #[error("failed to run git: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
}

#[derive(Debug, Error)]
pub enum CommitAllError {
    #[error("failed to run git: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
    #[error("nothing to commit")]
    NothingToCommit,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGit;

impl Git for SystemGit {
    fn commit_for_path_excluding(
        &self,
        path: &Path,
        excludes: &[&Path],
    ) -> Result<String, CommitForPathError> {
        let cwd = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };

        // A repository with no commits at all (an "unborn" branch) has no
        // HEAD for `git log` to walk, so it fails outright rather than
        // reporting an empty match — check for that case first (but only
        // inside an actual repository, so a path outside any repo still
        // reports `CommandFailed`) and treat it the same as "no commit
        // touches this path".
        if self.is_repository(cwd) {
            let head_exists = Command::new("git")
                .current_dir(cwd)
                .args(["rev-parse", "--verify", "-q", "HEAD"])
                .output()
                .map_err(|source| CommitForPathError::Spawn { source })?
                .status
                .success();
            if !head_exists {
                return Err(CommitForPathError::NotTracked {
                    path: path.to_path_buf(),
                });
            }
        }

        let mut command = Command::new("git");
        command
            .current_dir(cwd)
            .args(["log", "-1", "--format=%H", "--"])
            .arg(path);
        for exclude in excludes {
            let mut pathspec = std::ffi::OsString::from(":(exclude)");
            pathspec.push(exclude);
            command.arg(pathspec);
        }

        let output = command
            .output()
            .map_err(|source| CommitForPathError::Spawn { source })?;

        if !output.status.success() {
            return Err(CommitForPathError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hash.is_empty() {
            return Err(CommitForPathError::NotTracked {
                path: path.to_path_buf(),
            });
        }

        Ok(hash)
    }

    fn is_repository(&self, dir: &Path) -> bool {
        Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn init_repository(&self, dir: &Path) -> Result<(), InitRepositoryError> {
        let output = Command::new("git")
            .current_dir(dir)
            .arg("init")
            .output()
            .map_err(|source| InitRepositoryError::Spawn { source })?;

        if !output.status.success() {
            return Err(InitRepositoryError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(())
    }

    fn changed_paths(&self, dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain=v1", "--untracked-files=all"])
            .output()
            .map_err(|source| ChangedPathsError::Spawn { source })?;

        if !output.status.success() {
            return Err(ChangedPathsError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let paths = stdout
            .lines()
            .filter_map(|line| {
                // Each line is "XY <path>" or, for a detected rename,
                // "XY <old> -> <new>" — the status codes always occupy the
                // first two columns followed by a space, per `git status
                // --porcelain`'s documented (stable) format.
                let rest = line.get(3..)?;
                let path = rest.rsplit(" -> ").next().unwrap_or(rest);
                Some(PathBuf::from(path))
            })
            .collect();

        Ok(paths)
    }

    fn commit_all(&self, dir: &Path, message: &str) -> Result<(), CommitAllError> {
        if self.changed_paths(dir).map_or(true, |paths| paths.is_empty()) {
            return Err(CommitAllError::NothingToCommit);
        }

        let add_output = Command::new("git")
            .current_dir(dir)
            .args(["add", "-A"])
            .output()
            .map_err(|source| CommitAllError::Spawn { source })?;
        if !add_output.status.success() {
            return Err(CommitAllError::CommandFailed {
                status: add_output.status,
                stderr: String::from_utf8_lossy(&add_output.stderr).into_owned(),
            });
        }

        let commit_output = Command::new("git")
            .current_dir(dir)
            .args(["commit", "-m", message])
            .output()
            .map_err(|source| CommitAllError::Spawn { source })?;
        if !commit_output.status.success() {
            return Err(CommitAllError::CommandFailed {
                status: commit_output.status,
                stderr: String::from_utf8_lossy(&commit_output.stderr).into_owned(),
            });
        }

        Ok(())
    }
}

/// Wraps another `Git`, letting tests force `commit_for_path` on specific
/// paths to fail instead of shelling out to real git state.
#[derive(Debug, Default)]
pub struct FaultInjectingGit<G> {
    inner: G,
    faults: HashMap<PathBuf, io::ErrorKind>,
}

impl<G: Git> FaultInjectingGit<G> {
    pub fn new(inner: G) -> Self {
        Self {
            inner,
            faults: HashMap::new(),
        }
    }

    /// Every call touching `path` will fail until removed.
    pub fn inject(&mut self, path: impl Into<PathBuf>, kind: io::ErrorKind) {
        self.faults.insert(path.into(), kind);
    }

    pub fn clear(&mut self, path: &Path) {
        self.faults.remove(path);
    }
}

impl<G: Git> Git for FaultInjectingGit<G> {
    fn commit_for_path_excluding(
        &self,
        path: &Path,
        excludes: &[&Path],
    ) -> Result<String, CommitForPathError> {
        if let Some(kind) = self.faults.get(path) {
            return Err(CommitForPathError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", path.display())),
            });
        }
        self.inner.commit_for_path_excluding(path, excludes)
    }

    fn changed_paths(&self, dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
        self.inner.changed_paths(dir)
    }

    fn commit_all(&self, dir: &Path, message: &str) -> Result<(), CommitAllError> {
        self.inner.commit_all(dir, message)
    }
}

pub trait RemoteGit {
    /// The commit currently at `path` (or, if `path` is `None`, at the
    /// repository's `HEAD`) in the git repository at `url`. `path`, when
    /// given, may be a file or a directory — a directory resolves to the
    /// newest commit touching anything under it, same as `Git::commit_for_path`.
    fn commit_for_remote(
        &self,
        url: &str,
        path: Option<&Path>,
    ) -> Result<String, CommitForRemoteError>;
}

#[derive(Debug, Error)]
pub enum CommitForRemoteError {
    #[error("failed to run git: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
    #[error("repository {url} has no commits")]
    Empty { url: String },
    #[error("no commit touches {path} in {url}")]
    NotTracked { url: String, path: PathBuf },
}

impl RemoteGit for SystemGit {
    fn commit_for_remote(
        &self,
        url: &str,
        path: Option<&Path>,
    ) -> Result<String, CommitForRemoteError> {
        match path {
            None => commit_for_remote_head(url),
            Some(path) => commit_for_remote_path(url, path),
        }
    }
}

fn commit_for_remote_head(url: &str) -> Result<String, CommitForRemoteError> {
    let output = Command::new("git")
        .args(["ls-remote", url, "HEAD"])
        .output()
        .map_err(|source| CommitForRemoteError::Spawn { source })?;

    if !output.status.success() {
        return Err(CommitForRemoteError::CommandFailed {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let hash = stdout.split_whitespace().next().unwrap_or("");
    if hash.is_empty() {
        return Err(CommitForRemoteError::Empty {
            url: url.to_string(),
        });
    }

    Ok(hash.to_string())
}

fn commit_for_remote_path(url: &str, path: &Path) -> Result<String, CommitForRemoteError> {
    let clone_dir = std::env::temp_dir().join(format!(
        "syscalls-remote-git-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    ));

    let result = clone_and_look_up(url, &clone_dir, path);
    std::fs::remove_dir_all(&clone_dir).ok();
    result
}

fn clone_and_look_up(
    url: &str,
    clone_dir: &Path,
    path: &Path,
) -> Result<String, CommitForRemoteError> {
    let clone_output = Command::new("git")
        .args(["clone", "--quiet", url])
        .arg(clone_dir)
        .output()
        .map_err(|source| CommitForRemoteError::Spawn { source })?;

    if !clone_output.status.success() {
        return Err(CommitForRemoteError::CommandFailed {
            status: clone_output.status,
            stderr: String::from_utf8_lossy(&clone_output.stderr).into_owned(),
        });
    }

    let log_output = Command::new("git")
        .current_dir(clone_dir)
        .args(["log", "-1", "--format=%H", "--"])
        .arg(path)
        .output()
        .map_err(|source| CommitForRemoteError::Spawn { source })?;

    if !log_output.status.success() {
        return Err(CommitForRemoteError::CommandFailed {
            status: log_output.status,
            stderr: String::from_utf8_lossy(&log_output.stderr).into_owned(),
        });
    }

    let hash = String::from_utf8_lossy(&log_output.stdout)
        .trim()
        .to_string();
    if hash.is_empty() {
        return Err(CommitForRemoteError::NotTracked {
            url: url.to_string(),
            path: path.to_path_buf(),
        });
    }

    Ok(hash)
}

/// Wraps another `RemoteGit`, letting tests force `commit_for_remote` on
/// specific URLs to fail instead of shelling out to real git/network state.
#[derive(Debug, Default)]
pub struct FaultInjectingRemoteGit<G> {
    inner: G,
    faults: HashMap<String, io::ErrorKind>,
}

impl<G: RemoteGit> FaultInjectingRemoteGit<G> {
    pub fn new(inner: G) -> Self {
        Self {
            inner,
            faults: HashMap::new(),
        }
    }

    /// Every call touching `url` will fail until removed.
    pub fn inject(&mut self, url: impl Into<String>, kind: io::ErrorKind) {
        self.faults.insert(url.into(), kind);
    }

    pub fn clear(&mut self, url: &str) {
        self.faults.remove(url);
    }
}

impl<G: RemoteGit> RemoteGit for FaultInjectingRemoteGit<G> {
    fn commit_for_remote(
        &self,
        url: &str,
        path: Option<&Path>,
    ) -> Result<String, CommitForRemoteError> {
        if let Some(kind) = self.faults.get(url) {
            return Err(CommitForRemoteError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {url}")),
            });
        }
        self.inner.commit_for_remote(url, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn std_filesystem_round_trips_a_file() {
        let dir = std::env::temp_dir().join(format!("syscalls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");

        let fs = StdFilesystem;
        fs.write(&file, b"hello").unwrap();
        assert_eq!(fs.read_to_string(&file).unwrap(), "hello");
        assert_eq!(fs.read(&file).unwrap(), b"hello");
        assert!(fs.exists(&file));
        assert!(fs.is_dir(&dir));

        fs.remove_dir_all(&dir).unwrap();
        assert!(!fs.exists(&dir));
    }

    #[test]
    fn slow_filesystem_delays_writes_but_not_reads() {
        let dir = std::env::temp_dir().join(format!("syscalls-slow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");

        let fs = SlowFilesystem::new(StdFilesystem, Duration::from_millis(50));

        let before_write = std::time::Instant::now();
        fs.write(&file, b"hello").unwrap();
        assert!(before_write.elapsed() >= Duration::from_millis(50));

        let before_read = std::time::Instant::now();
        assert_eq!(fs.read_to_string(&file).unwrap(), "hello");
        assert!(before_read.elapsed() < Duration::from_millis(50));

        assert!(fs.exists(&file));
        assert!(fs.is_dir(&dir));
        assert_eq!(fs.read(&file).unwrap(), b"hello");
        assert_eq!(fs.read_dir(&dir).unwrap(), vec![file.clone()]);

        let subdir = dir.join("nested");
        let before_create = std::time::Instant::now();
        fs.create_dir_all(&subdir).unwrap();
        assert!(before_create.elapsed() >= Duration::from_millis(50));
        assert!(fs.is_dir(&subdir));

        let before_remove = std::time::Instant::now();
        fs.remove_dir_all(&dir).unwrap();
        assert!(before_remove.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn fault_injection_overrides_read() {
        let dir = std::env::temp_dir().join(format!("syscalls-fault-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        let mut real = std::fs::File::create(&file).unwrap();
        real.write_all(b"hello").unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&file, io::ErrorKind::PermissionDenied);

        let err = fs.read_to_string(&file).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        fs.clear(&file);
        assert_eq!(fs.read_to_string(&file).unwrap(), "hello");

        fs.inject(&dir, io::ErrorKind::PermissionDenied);
        let err = fs.remove_dir_all(&dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        fs.clear(&dir);
        fs.remove_dir_all(&dir).unwrap();
    }

    /// Sets up an isolated scratch git repo (not this repository's own
    /// history) with a single tracked, committed file.
    fn scratch_git_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-{name}-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let run = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };

        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("tracked.txt"), "hello").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "--quiet", "-m", "initial"]);

        dir
    }

    #[test]
    fn system_git_returns_the_commit_for_a_tracked_path() {
        let dir = scratch_git_repo("tracked");

        let expected = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let hash = SystemGit.commit_for_path(&dir.join("tracked.txt")).unwrap();
        assert_eq!(hash, expected);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_git_is_repository_is_true_inside_a_repo() {
        let dir = scratch_git_repo("is-repository-true");

        assert!(SystemGit.is_repository(&dir));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_git_is_repository_is_false_outside_a_repo() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-is-repository-false-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!SystemGit.is_repository(&dir));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_git_init_repository_makes_is_repository_true() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-init-repository-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!SystemGit.is_repository(&dir));
        SystemGit.init_repository(&dir).unwrap();
        assert!(SystemGit.is_repository(&dir));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_for_path_excluding_ignores_commits_confined_to_the_excluded_path() {
        let dir = scratch_git_repo("excluding-ignores");

        let run = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };

        let initial_commit = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inner.txt"), "hello").unwrap();
        run(&["add", "sub/inner.txt"]);
        run(&["commit", "--quiet", "-m", "touches only sub/"]);

        let unexcluded = SystemGit.commit_for_path(&dir).unwrap();
        assert_ne!(unexcluded, initial_commit);

        let excluded = SystemGit
            .commit_for_path_excluding(&dir, &[&dir.join("sub")])
            .unwrap();
        assert_eq!(excluded, initial_commit);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_for_path_excluding_reports_not_tracked_when_only_the_excluded_path_has_commits() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-excluding-not-tracked-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        let run = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };

        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("sub/inner.txt"), "hello").unwrap();
        run(&["add", "sub/inner.txt"]);
        run(&["commit", "--quiet", "-m", "touches only sub/"]);

        let err = SystemGit
            .commit_for_path_excluding(&dir, &[&dir.join("sub")])
            .unwrap_err();
        assert!(matches!(err, CommitForPathError::NotTracked { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_git_reports_untracked_paths() {
        let dir = scratch_git_repo("untracked");
        std::fs::write(dir.join("untracked.txt"), "hello").unwrap();

        let err = SystemGit
            .commit_for_path(&dir.join("untracked.txt"))
            .unwrap_err();
        assert!(matches!(err, CommitForPathError::NotTracked { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_git_reports_not_tracked_for_a_repo_with_no_commits_at_all() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-unborn-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("git")
            .current_dir(&dir)
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(dir.join("file.txt"), "hello").unwrap();

        let err = SystemGit.commit_for_path(&dir.join("file.txt")).unwrap_err();
        assert!(matches!(err, CommitForPathError::NotTracked { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn changed_paths_reports_nothing_in_a_clean_repo() {
        let dir = scratch_git_repo("changed-paths-clean");

        assert_eq!(SystemGit.changed_paths(&dir).unwrap(), Vec::<PathBuf>::new());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn changed_paths_reports_modified_and_untracked_paths() {
        let dir = scratch_git_repo("changed-paths-mixed");
        std::fs::write(dir.join("tracked.txt"), "modified").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/new.txt"), "new").unwrap();
        std::fs::write(dir.join("untracked.txt"), "untracked").unwrap();

        let mut paths = SystemGit.changed_paths(&dir).unwrap();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("sub/new.txt"),
                PathBuf::from("tracked.txt"),
                PathBuf::from("untracked.txt"),
            ]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_all_reports_nothing_to_commit_in_a_clean_repo() {
        let dir = scratch_git_repo("commit-all-clean");

        let err = SystemGit.commit_all(&dir, "nothing pending").unwrap_err();
        assert!(matches!(err, CommitAllError::NothingToCommit));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_all_stages_and_commits_every_pending_change() {
        let dir = scratch_git_repo("commit-all-mixed");
        std::fs::write(dir.join("tracked.txt"), "modified").unwrap();
        std::fs::write(dir.join("untracked.txt"), "untracked").unwrap();

        SystemGit.commit_all(&dir, "commit everything").unwrap();

        assert_eq!(SystemGit.changed_paths(&dir).unwrap(), Vec::<PathBuf>::new());
        let log = Command::new("git")
            .current_dir(&dir)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "commit everything");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_git_reports_paths_outside_any_repo() {
        let dir = std::env::temp_dir().join(format!("syscalls-non-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), "hello").unwrap();

        let err = SystemGit
            .commit_for_path(&dir.join("file.txt"))
            .unwrap_err();
        assert!(matches!(err, CommitForPathError::CommandFailed { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_commit_for_path() {
        let dir = scratch_git_repo("fault");
        let path = dir.join("tracked.txt");

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&path, io::ErrorKind::PermissionDenied);

        let err = git.commit_for_path(&path).unwrap_err();
        assert!(matches!(err, CommitForPathError::Spawn { .. }));

        git.clear(&path);
        assert!(git.commit_for_path(&path).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn file_url(dir: &Path) -> String {
        format!("file://{}", dir.display())
    }

    #[test]
    fn system_remote_git_returns_head_when_no_path_is_given() {
        let dir = scratch_git_repo("remote-head");

        let expected = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let hash = SystemGit.commit_for_remote(&file_url(&dir), None).unwrap();
        assert_eq!(hash, expected);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_remote_git_returns_the_commit_for_a_tracked_file() {
        let dir = scratch_git_repo("remote-tracked");

        let expected = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let hash = SystemGit
            .commit_for_remote(&file_url(&dir), Some(Path::new("tracked.txt")))
            .unwrap();
        assert_eq!(hash, expected);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_remote_git_returns_the_newest_commit_in_a_nested_directory() {
        let dir = scratch_git_repo("remote-nested");

        std::fs::create_dir_all(dir.join("nested/inner")).unwrap();
        std::fs::write(dir.join("nested/inner/file.txt"), "hello").unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["add", "nested/inner/file.txt"]);
        run(&["commit", "--quiet", "-m", "nested"]);

        let expected = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let hash = SystemGit
            .commit_for_remote(&file_url(&dir), Some(Path::new("nested")))
            .unwrap();
        assert_eq!(hash, expected);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_remote_git_reports_untracked_paths() {
        let dir = scratch_git_repo("remote-untracked");

        let err = SystemGit
            .commit_for_remote(&file_url(&dir), Some(Path::new("untracked.txt")))
            .unwrap_err();
        assert!(matches!(err, CommitForRemoteError::NotTracked { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_remote_git_reports_bad_urls() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-remote-git-missing-{}-{}",
            std::process::id(),
            line!()
        ));

        let err = SystemGit
            .commit_for_remote(&file_url(&dir), None)
            .unwrap_err();
        assert!(matches!(err, CommitForRemoteError::CommandFailed { .. }));
    }

    #[test]
    fn fault_injecting_remote_git_overrides_commit_for_remote() {
        let dir = scratch_git_repo("remote-fault");
        let url = file_url(&dir);

        let mut git = FaultInjectingRemoteGit::new(SystemGit);
        git.inject(url.clone(), io::ErrorKind::PermissionDenied);

        let err = git.commit_for_remote(&url, None).unwrap_err();
        assert!(matches!(err, CommitForRemoteError::Spawn { .. }));

        git.clear(&url);
        assert!(git.commit_for_remote(&url, None).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
