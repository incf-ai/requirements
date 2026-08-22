use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use thiserror::Error;

pub trait Filesystem {
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn is_dir(&self, path: &Path) -> bool;
    fn exists(&self, path: &Path) -> bool;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
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
}

pub trait Git {
    fn commit_for_path(&self, path: &Path) -> Result<String, CommitForPathError>;
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

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGit;

impl Git for SystemGit {
    fn commit_for_path(&self, path: &Path) -> Result<String, CommitForPathError> {
        let cwd = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };

        let output = Command::new("git")
            .current_dir(cwd)
            .args(["log", "-1", "--format=%H", "--"])
            .arg(path)
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
    fn commit_for_path(&self, path: &Path) -> Result<String, CommitForPathError> {
        if let Some(kind) = self.faults.get(path) {
            return Err(CommitForPathError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", path.display())),
            });
        }
        self.inner.commit_for_path(path)
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

        std::fs::remove_dir_all(&dir).unwrap();
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

        std::fs::remove_dir_all(&dir).unwrap();
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
    fn system_git_reports_paths_outside_any_repo() {
        let dir = std::env::temp_dir().join(format!("syscalls-non-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), "hello").unwrap();

        let err = SystemGit.commit_for_path(&dir.join("file.txt")).unwrap_err();
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
}
