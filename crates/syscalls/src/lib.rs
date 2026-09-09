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

    /// Every commit touching `path`, newest first, up to `limit` — the full
    /// history `commit_for_path_excluding` only ever returns the newest
    /// entry of. Same `excludes` semantics. Unlike
    /// `commit_for_path_excluding`, an empty result is never an error: a
    /// path with no matching commits (or a repository with no commits at
    /// all yet) is simply a history of length zero, not something
    /// exceptional the way "no *latest* commit" is.
    ///
    /// Defaults to a single-entry log built from `commit_for_path_excluding`
    /// (empty subject/date, since the single-hash lookup doesn't have
    /// them; empty log once that reports `NotTracked`) so the workspace's
    /// several hand-rolled test fakes, which only implement "what's the
    /// latest commit", don't also need their own override.
    fn commit_log_for_path(
        &self,
        path: &Path,
        excludes: &[&Path],
        _limit: usize,
    ) -> Result<Vec<CommitInfo>, CommitForPathError> {
        match self.commit_for_path_excluding(path, excludes) {
            Ok(hash) => Ok(vec![CommitInfo {
                hash,
                subject: String::new(),
                date: String::new(),
            }]),
            Err(e) if e.is_not_tracked() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Every file `commit` itself changed under `dir`, relative to `dir`,
    /// ignoring any confined entirely to `excludes` — the "Commit history"
    /// section's per-commit file list, scoped to agree with whatever
    /// `commit_log_for_path` excluded when it produced `commit` in the
    /// first place (a file that couldn't have produced a log entry
    /// shouldn't appear in that entry's own file list either). An empty
    /// result (a commit that touched nothing under `dir` once `excludes`
    /// is applied) is a normal answer, not an error.
    ///
    /// Defaults to `Ok(Vec::new())`, same "trivial default so hand-rolled
    /// test fakes don't need their own override" reasoning as `push`'s
    /// existing default.
    fn files_changed_in_commit(
        &self,
        _dir: &Path,
        _commit: &str,
        _excludes: &[&Path],
    ) -> Result<Vec<PathBuf>, CommitForPathError> {
        Ok(Vec::new())
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

    /// A unified diff for `path` (relative to `dir`, as `changed_paths`
    /// itself returns them) against what a `commit_all` would compare it
    /// to — `HEAD` for a tracked file, or an empty tree for one that's
    /// untracked/has no `HEAD` yet to diff against. Read-only: never stages
    /// anything, so calling this doesn't change what a subsequent
    /// `commit_all` would sweep up.
    fn diff(&self, dir: &Path, path: &Path) -> Result<String, DiffError>;

    /// The diff `commit` itself introduced for `path` (relative to `dir`,
    /// as `files_changed_in_commit` itself returns them) — parent vs.
    /// `commit`, or an empty tree vs. `commit` for a root commit with no
    /// parent (`git show`'s own native behavior for that case, unlike
    /// `diff`'s own manual `--no-index` fallback for an untracked file).
    /// Read-only, same as `diff`.
    ///
    /// Defaults to `self.diff(dir, path)` (ignoring `commit` entirely) —
    /// the workspace's hand-rolled test fakes only assert plumbing/shape
    /// against this, never real git semantics, so this trivial default
    /// (same reasoning as `push`'s) lets them skip their own override.
    fn diff_for_commit(&self, dir: &Path, _commit: &str, path: &Path) -> Result<String, DiffError> {
        self.diff(dir, path)
    }

    /// Pushes `dir`'s current branch to its configured upstream (`git
    /// push`, no explicit remote/branch — relies on the repo's own
    /// tracking branch). Returns the combined stdout+stderr text on
    /// success as a human-readable status to show the user — git's
    /// ref-update summary ("main -> main", "Everything up-to-date", etc.)
    /// is printed almost entirely to stderr even when the push succeeds.
    /// Defaults to a no-op success so fakes that don't care about pushing
    /// don't need their own override, same reasoning as
    /// `is_repository`/`init_repository`'s defaults above.
    fn push(&self, _dir: &Path) -> Result<String, PushError> {
        Ok(String::new())
    }

    /// Every local commit not yet on `dir`'s upstream (`git log
    /// @{u}..HEAD`), newest first — exactly what a `push` would actually
    /// send, so the push dialog can preview it before the user commits to
    /// a real push. An empty result is a normal answer (nothing to push),
    /// not an error. Defaults to an empty list so fakes that don't care
    /// about pushing don't need their own override, same reasoning as
    /// `push`'s own default.
    fn unpushed_commits(&self, _dir: &Path) -> Result<Vec<CommitInfo>, UnpushedCommitsError> {
        Ok(Vec::new())
    }
}

/// One entry of `Git::commit_log_for_path`'s result — a single commit's
/// hash, message subject (first line only, same as `git log`'s own
/// `%s`), and author date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub hash: String,
    pub subject: String,
    pub date: String,
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

impl CommitForPathError {
    /// True when `path` simply has no commit in history yet (rather than a
    /// genuine git failure) — the ordinary state for an entry that's been
    /// added in this editing session but not yet saved/committed, not
    /// something worth surfacing as an error.
    pub fn is_not_tracked(&self) -> bool {
        matches!(self, CommitForPathError::NotTracked { .. })
    }
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

#[derive(Debug, Error)]
pub enum DiffError {
    #[error("failed to run git: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
}

#[derive(Debug, Error)]
pub enum PushError {
    #[error("failed to run git: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
}

#[derive(Debug, Error)]
pub enum UnpushedCommitsError {
    #[error("failed to run git: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("git exited with {status}: {stderr}")]
    CommandFailed { status: ExitStatus, stderr: String },
}

/// `path` rewritten as a pathspec valid after `Command::current_dir(cwd)`
/// — `.` when `path` and `cwd` are the same directory (git rejects an
/// empty string as a pathspec, unlike `.`), the part of `path` beyond
/// `cwd` when `path` is nested under it, or `path` itself unchanged if
/// it isn't under `cwd` at all (doesn't happen for any real caller here,
/// but a fallback beats a panic on a `strip_prefix` that fails).
fn relative_pathspec<'a>(path: &'a Path, cwd: &Path) -> std::borrow::Cow<'a, Path> {
    match path.strip_prefix(cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => Path::new(".").into(),
        Ok(relative) => relative.into(),
        Err(_) => path.into(),
    }
}

/// The directory `git`'s pathspecs below should resolve relative to for a
/// lookup against `path` — `path` itself if it's a directory, its parent
/// otherwise. Shared by `commit_for_path_excluding` and
/// `commit_log_for_path` (both resolve `path`/`excludes` the same way).
fn log_cwd_for(path: &Path) -> &Path {
    if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    }
}

/// Whether `cwd` (inside an actual repository — checked by the caller via
/// `is_repository`) has a `HEAD` for `git log` to walk at all. A brand-new
/// repository with no commits yet (an "unborn" branch) doesn't, and `git
/// log`/`git rev-parse` fail outright rather than reporting an empty match
/// in that state — checking this up front lets both `commit_for_path_excluding`
/// and `commit_log_for_path` treat it as "nothing here yet" in whatever way
/// suits each (an error for the former, an empty log for the latter)
/// instead of a confusing `CommandFailed`.
fn head_exists(cwd: &Path) -> Result<bool, CommitForPathError> {
    Ok(Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output()
        .map_err(|source| CommitForPathError::Spawn { source })?
        .status
        .success())
}

/// Appends `-- <path> [:(exclude)<each of excludes>]` to `command`, with
/// every path re-relativized against `cwd` first — shared by
/// `commit_for_path_excluding` and `commit_log_for_path`, whose pathspec
/// handling is otherwise identical. See `relative_pathspec`'s own doc
/// comment for why re-relativizing against `cwd` (rather than passing
/// `path`/`excludes` through unchanged) matters for a relative `path`.
fn apply_path_and_excludes(command: &mut Command, path: &Path, excludes: &[&Path], cwd: &Path) {
    command
        .arg("--")
        .arg(relative_pathspec(path, cwd).as_ref());
    for exclude in excludes {
        let mut pathspec = std::ffi::OsString::from(":(exclude)");
        pathspec.push(relative_pathspec(exclude, cwd).as_ref());
        command.arg(pathspec);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGit;

impl Git for SystemGit {
    fn commit_for_path_excluding(
        &self,
        path: &Path,
        excludes: &[&Path],
    ) -> Result<String, CommitForPathError> {
        let cwd = log_cwd_for(path);

        // See `head_exists`'s own doc comment on why this is checked first
        // (but only inside an actual repository, so a path outside any
        // repo still reports `CommandFailed` rather than `NotTracked`).
        if self.is_repository(cwd) && !head_exists(cwd)? {
            return Err(CommitForPathError::NotTracked {
                path: path.to_path_buf(),
            });
        }

        let mut command = Command::new("git");
        command.current_dir(cwd).args(["log", "-1", "--format=%H"]);
        apply_path_and_excludes(&mut command, path, excludes, cwd);

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

    fn commit_log_for_path(
        &self,
        path: &Path,
        excludes: &[&Path],
        limit: usize,
    ) -> Result<Vec<CommitInfo>, CommitForPathError> {
        let cwd = log_cwd_for(path);

        // Unlike `commit_for_path_excluding`, an unborn HEAD (or, below, a
        // path with no matching commits) is simply an empty log here, not
        // an error — see this method's own doc comment.
        if self.is_repository(cwd) && !head_exists(cwd)? {
            return Ok(Vec::new());
        }

        // Fields are separated by `\x1f` (unit separator) rather than
        // anything printable, so a `%s` subject containing e.g. a literal
        // `|` or tab still splits unambiguously; each commit is already on
        // its own stdout line courtesy of `git log`'s own per-commit
        // formatting.
        let mut command = Command::new("git");
        command.current_dir(cwd).args([
            "log",
            &format!("-{limit}"),
            "--format=%H%x1f%s%x1f%ad",
            "--date=short",
        ]);
        apply_path_and_excludes(&mut command, path, excludes, cwd);

        let output = command
            .output()
            .map_err(|source| CommitForPathError::Spawn { source })?;

        if !output.status.success() {
            return Err(CommitForPathError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let commits = stdout
            .lines()
            .map(|line| {
                let mut fields = line.splitn(3, '\u{1f}');
                CommitInfo {
                    hash: fields.next().unwrap_or_default().to_string(),
                    subject: fields.next().unwrap_or_default().to_string(),
                    date: fields.next().unwrap_or_default().to_string(),
                }
            })
            .collect();

        Ok(commits)
    }

    fn files_changed_in_commit(
        &self,
        dir: &Path,
        commit: &str,
        excludes: &[&Path],
    ) -> Result<Vec<PathBuf>, CommitForPathError> {
        // `--relative` (no argument) defaults to paths relative to
        // `current_dir` — `dir` itself here, matching the convention every
        // other method on this trait uses (`dir` is always the git `cwd`).
        // `--format=` suppresses `git show`'s usual commit-message header,
        // leaving just the (here, `--name-only`) file list.
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .args(["show", "--format=", "--name-only", "--relative", commit]);
        apply_path_and_excludes(&mut command, dir, excludes, dir);

        let output = command
            .output()
            .map_err(|source| CommitForPathError::Spawn { source })?;

        if !output.status.success() {
            return Err(CommitForPathError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect())
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

    fn diff(&self, dir: &Path, path: &Path) -> Result<String, DiffError> {
        // `git diff HEAD -- <path>` only knows about paths already in the
        // index or `HEAD` — a genuinely untracked file (or any file at all
        // in a brand-new repo with no commits yet) shows up there as no
        // difference rather than as the new-file diff a user expects, so
        // that case is detected via `git status` first and routed through
        // `--no-index` against `/dev/null` instead, the same "treat it as
        // an addition" trick `git diff --no-index` is designed for.
        let status_output = Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain=v1", "--untracked-files=all", "--"])
            .arg(path)
            .output()
            .map_err(|source| DiffError::Spawn { source })?;
        if !status_output.status.success() {
            return Err(DiffError::CommandFailed {
                status: status_output.status,
                stderr: String::from_utf8_lossy(&status_output.stderr).into_owned(),
            });
        }
        let untracked = String::from_utf8_lossy(&status_output.stdout)
            .lines()
            .any(|line| line.starts_with("??"));

        let head_exists = !untracked
            && self.is_repository(dir)
            && Command::new("git")
                .current_dir(dir)
                .args(["rev-parse", "--verify", "-q", "HEAD"])
                .output()
                .map_err(|source| DiffError::Spawn { source })?
                .status
                .success();

        let output = if untracked || !head_exists {
            Command::new("git")
                .current_dir(dir)
                .args(["diff", "--no-index", "--", "/dev/null"])
                .arg(path)
                .output()
                .map_err(|source| DiffError::Spawn { source })?
        } else {
            Command::new("git")
                .current_dir(dir)
                .args(["diff", "HEAD", "--"])
                .arg(path)
                .output()
                .map_err(|source| DiffError::Spawn { source })?
        };

        // `git diff --no-index` uses exit-code semantics (0 = identical,
        // 1 = differences found, 2+ = real error) unlike ordinary `git
        // diff`, which always exits 0 regardless of what it finds — treat
        // exit code 1 as success here too rather than a command failure.
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(DiffError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn diff_for_commit(&self, dir: &Path, commit: &str, path: &Path) -> Result<String, DiffError> {
        // Unlike `diff`, no untracked-file/`--no-index` handling is needed
        // here — `commit` always names a real commit already in history,
        // and `git show` already diffs a root commit (no parent) against
        // an empty tree on its own, the same "treat it as an addition"
        // outcome `diff`'s manual fallback exists to get for a *working
        // tree* file with no `HEAD` to compare against.
        let output = Command::new("git")
            .current_dir(dir)
            .args(["show", "--format=", commit, "--"])
            .arg(path)
            .output()
            .map_err(|source| DiffError::Spawn { source })?;

        if !output.status.success() {
            return Err(DiffError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn push(&self, dir: &Path) -> Result<String, PushError> {
        let output = Command::new("git")
            .current_dir(dir)
            .arg("push")
            .output()
            .map_err(|source| PushError::Spawn { source })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(PushError::CommandFailed {
                status: output.status,
                stderr: stderr.into_owned(),
            });
        }

        Ok(format!("{stdout}{stderr}"))
    }

    fn unpushed_commits(&self, dir: &Path) -> Result<Vec<CommitInfo>, UnpushedCommitsError> {
        // Same `\x1f`-separated `--format` as `commit_log_for_path` — see
        // that method's own doc comment on why.
        let output = Command::new("git")
            .current_dir(dir)
            .args([
                "log",
                "@{u}..HEAD",
                "--format=%H%x1f%s%x1f%ad",
                "--date=short",
            ])
            .output()
            .map_err(|source| UnpushedCommitsError::Spawn { source })?;

        if !output.status.success() {
            return Err(UnpushedCommitsError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .map(|line| {
                let mut fields = line.splitn(3, '\u{1f}');
                CommitInfo {
                    hash: fields.next().unwrap_or_default().to_string(),
                    subject: fields.next().unwrap_or_default().to_string(),
                    date: fields.next().unwrap_or_default().to_string(),
                }
            })
            .collect())
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

    fn commit_log_for_path(
        &self,
        path: &Path,
        excludes: &[&Path],
        limit: usize,
    ) -> Result<Vec<CommitInfo>, CommitForPathError> {
        if let Some(kind) = self.faults.get(path) {
            return Err(CommitForPathError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", path.display())),
            });
        }
        self.inner.commit_log_for_path(path, excludes, limit)
    }

    /// Delegates to `inner` rather than the trait's `true` default — unlike
    /// every other method here, there's no `Result` to carry an injected
    /// fault, so an injected fault instead makes this report `dir` as not a
    /// repository (the closest bool-shaped equivalent).
    fn is_repository(&self, dir: &Path) -> bool {
        if self.faults.contains_key(dir) {
            return false;
        }
        self.inner.is_repository(dir)
    }

    fn init_repository(&self, dir: &Path) -> Result<(), InitRepositoryError> {
        if let Some(kind) = self.faults.get(dir) {
            return Err(InitRepositoryError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", dir.display())),
            });
        }
        self.inner.init_repository(dir)
    }

    fn changed_paths(&self, dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
        if let Some(kind) = self.faults.get(dir) {
            return Err(ChangedPathsError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", dir.display())),
            });
        }
        self.inner.changed_paths(dir)
    }

    fn commit_all(&self, dir: &Path, message: &str) -> Result<(), CommitAllError> {
        if let Some(kind) = self.faults.get(dir) {
            return Err(CommitAllError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", dir.display())),
            });
        }
        self.inner.commit_all(dir, message)
    }

    /// Keyed on `dir`, same convention as `changed_paths`/`commit_all`
    /// (this takes no single "the" path the way `diff`/`commit_for_path_excluding` do).
    fn files_changed_in_commit(
        &self,
        dir: &Path,
        commit: &str,
        excludes: &[&Path],
    ) -> Result<Vec<PathBuf>, CommitForPathError> {
        if let Some(kind) = self.faults.get(dir) {
            return Err(CommitForPathError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", dir.display())),
            });
        }
        self.inner.files_changed_in_commit(dir, commit, excludes)
    }

    /// Checks `path` (the more specific of the two, matching
    /// `commit_for_path_excluding`'s convention) rather than `dir`.
    fn diff(&self, dir: &Path, path: &Path) -> Result<String, DiffError> {
        if let Some(kind) = self.faults.get(path) {
            return Err(DiffError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", path.display())),
            });
        }
        self.inner.diff(dir, path)
    }

    /// Checks `path`, same convention as `diff` itself.
    fn diff_for_commit(&self, dir: &Path, commit: &str, path: &Path) -> Result<String, DiffError> {
        if let Some(kind) = self.faults.get(path) {
            return Err(DiffError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", path.display())),
            });
        }
        self.inner.diff_for_commit(dir, commit, path)
    }

    fn push(&self, dir: &Path) -> Result<String, PushError> {
        if let Some(kind) = self.faults.get(dir) {
            return Err(PushError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", dir.display())),
            });
        }
        self.inner.push(dir)
    }

    /// Keyed on `dir`, same convention as `push` itself.
    fn unpushed_commits(&self, dir: &Path) -> Result<Vec<CommitInfo>, UnpushedCommitsError> {
        if let Some(kind) = self.faults.get(dir) {
            return Err(UnpushedCommitsError::Spawn {
                source: io::Error::new(*kind, format!("injected fault for {}", dir.display())),
            });
        }
        self.inner.unpushed_commits(dir)
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

impl CommitForRemoteError {
    /// Same "not an error, just not committed yet" case as
    /// `CommitForPathError::is_not_tracked`.
    pub fn is_not_tracked(&self) -> bool {
        matches!(self, CommitForRemoteError::NotTracked { .. })
    }
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

    /// A caller passing a path relative to *its own* cwd (e.g. `cli --dir
    /// test_project`, run from the workspace root) used to get a spurious
    /// `NotTracked`: `commit_for_path_excluding` runs `git` from `cwd` =
    /// `path` itself (a directory), a location generally different from
    /// the calling process's own cwd, but re-passed `path` (and each
    /// exclude) to `git` unchanged — resolved a second time against that
    /// new `cwd`, doubling the prefix and matching nothing. Reproduced
    /// here by actually changing the process's cwd to `dir`'s *parent* and
    /// passing `dir`'s bare name (and `sub` beneath it) as relative paths,
    /// the same shape a relative `--dir` produces.
    #[test]
    fn commit_for_path_excluding_works_with_a_relative_path_and_relative_excludes() {
        let dir = scratch_git_repo("relative-path");

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

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.parent().unwrap()).unwrap();
        let relative_dir = PathBuf::from(dir.file_name().unwrap());
        let result =
            SystemGit.commit_for_path_excluding(&relative_dir, &[&relative_dir.join("sub")]);
        std::env::set_current_dir(&original_cwd).unwrap();

        assert_eq!(result.unwrap(), initial_commit);

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
    fn commit_log_for_path_returns_every_commit_newest_first() {
        let dir = scratch_git_repo("log-newest-first");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::write(dir.join("tracked.txt"), "second").unwrap();
        run(&["commit", "--quiet", "-am", "second commit"]);
        std::fs::write(dir.join("tracked.txt"), "third").unwrap();
        run(&["commit", "--quiet", "-am", "third commit"]);

        let log = SystemGit
            .commit_log_for_path(&dir.join("tracked.txt"), &[], 10)
            .unwrap();

        assert_eq!(log.len(), 3);
        assert_eq!(log[0].subject, "third commit");
        assert_eq!(log[1].subject, "second commit");
        assert_eq!(log[2].subject, "initial");
        assert!(log.iter().all(|c| !c.hash.is_empty() && !c.date.is_empty()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_log_for_path_respects_limit() {
        let dir = scratch_git_repo("log-limit");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::write(dir.join("tracked.txt"), "second").unwrap();
        run(&["commit", "--quiet", "-am", "second commit"]);
        std::fs::write(dir.join("tracked.txt"), "third").unwrap();
        run(&["commit", "--quiet", "-am", "third commit"]);

        let log = SystemGit
            .commit_log_for_path(&dir.join("tracked.txt"), &[], 2)
            .unwrap();

        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "third commit");
        assert_eq!(log[1].subject, "second commit");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_log_for_path_excluding_ignores_commits_confined_to_the_excluded_path() {
        let dir = scratch_git_repo("log-excluding-ignores");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inner.txt"), "hello").unwrap();
        run(&["add", "sub/inner.txt"]);
        run(&["commit", "--quiet", "-m", "touches only sub/"]);

        let unexcluded = SystemGit.commit_log_for_path(&dir, &[], 10).unwrap();
        assert_eq!(unexcluded.len(), 2);

        let excluded = SystemGit
            .commit_log_for_path(&dir, &[&dir.join("sub")], 10)
            .unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].subject, "initial");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_log_for_path_is_empty_for_a_repo_with_no_commits_at_all() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-log-unborn-{}-{}",
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

        let log = SystemGit.commit_log_for_path(&dir.join("file.txt"), &[], 10).unwrap();
        assert!(log.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_log_for_path_is_empty_for_an_untracked_path_in_a_repo_with_history() {
        let dir = scratch_git_repo("log-untracked");
        std::fs::write(dir.join("untracked.txt"), "hello").unwrap();

        let log = SystemGit
            .commit_log_for_path(&dir.join("untracked.txt"), &[], 10)
            .unwrap();
        assert!(log.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_commit_log_for_path() {
        let dir = scratch_git_repo("fault-log");
        let path = dir.join("tracked.txt");

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&path, io::ErrorKind::PermissionDenied);

        let err = git.commit_log_for_path(&path, &[], 10).unwrap_err();
        assert!(matches!(err, CommitForPathError::Spawn { .. }));

        git.clear(&path);
        assert!(git.commit_log_for_path(&path, &[], 10).unwrap().len() == 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs `git rev-parse HEAD` in `dir` and returns the trimmed hash —
    /// shared by the `files_changed_in_commit`/`diff_for_commit` tests
    /// below, which (unlike `commit_log_for_path`'s tests) need a specific
    /// commit hash to ask about, not just "the log".
    fn head_hash(dir: &Path) -> String {
        String::from_utf8(
            Command::new("git")
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

    #[test]
    fn files_changed_in_commit_lists_only_what_that_commit_touched() {
        let dir = scratch_git_repo("files-changed-only-this-commit");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::write(dir.join("second.txt"), "second").unwrap();
        run(&["add", "second.txt"]);
        run(&["commit", "--quiet", "-m", "second commit"]);
        let second_commit = head_hash(&dir);

        let files = SystemGit
            .files_changed_in_commit(&dir, &second_commit, &[])
            .unwrap();
        assert_eq!(files, vec![PathBuf::from("second.txt")]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_changed_in_commit_lists_files_for_a_root_commit_with_no_parent() {
        let dir = scratch_git_repo("files-changed-root-commit");
        let root_commit = head_hash(&dir);

        let files = SystemGit
            .files_changed_in_commit(&dir, &root_commit, &[])
            .unwrap();
        assert_eq!(files, vec![PathBuf::from("tracked.txt")]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_changed_in_commit_excludes_a_confined_subtree() {
        let dir = scratch_git_repo("files-changed-excluding");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inner.txt"), "hello").unwrap();
        std::fs::write(dir.join("second.txt"), "second").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "--quiet", "-m", "touches both sub/ and a top-level file"]);
        let commit = head_hash(&dir);

        let files = SystemGit
            .files_changed_in_commit(&dir, &commit, &[&dir.join("sub")])
            .unwrap();
        assert_eq!(files, vec![PathBuf::from("second.txt")]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_for_commit_returns_what_that_commit_changed() {
        let dir = scratch_git_repo("diff-for-commit-changed");
        std::fs::write(dir.join("tracked.txt"), "goodbye").unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["commit", "--quiet", "-am", "modify tracked.txt"]);
        let commit = head_hash(&dir);

        let diff = SystemGit
            .diff_for_commit(&dir, &commit, Path::new("tracked.txt"))
            .unwrap();
        assert!(diff.contains("-hello"), "diff was: {diff}");
        assert!(diff.contains("+goodbye"), "diff was: {diff}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_for_commit_is_empty_for_a_file_that_commit_did_not_touch() {
        let dir = scratch_git_repo("diff-for-commit-untouched");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::write(dir.join("second.txt"), "second").unwrap();
        run(&["add", "second.txt"]);
        run(&["commit", "--quiet", "-m", "add second.txt only"]);
        let commit = head_hash(&dir);

        let diff = SystemGit
            .diff_for_commit(&dir, &commit, Path::new("tracked.txt"))
            .unwrap();
        assert_eq!(diff, "");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_files_changed_in_commit() {
        let dir = scratch_git_repo("fault-files-changed");
        let commit = head_hash(&dir);

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&dir, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.files_changed_in_commit(&dir, &commit, &[]).unwrap_err(),
            CommitForPathError::Spawn { .. }
        ));

        git.clear(&dir);
        assert!(git.files_changed_in_commit(&dir, &commit, &[]).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_diff_for_commit() {
        let dir = scratch_git_repo("fault-diff-for-commit");
        let path = dir.join("tracked.txt");
        let commit = head_hash(&dir);

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&path, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.diff_for_commit(&dir, &commit, &path).unwrap_err(),
            DiffError::Spawn { .. }
        ));

        git.clear(&path);
        assert!(git.diff_for_commit(&dir, &commit, &path).is_ok());

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
    fn diff_reports_a_modified_tracked_file_against_head() {
        let dir = scratch_git_repo("diff-modified");
        std::fs::write(dir.join("tracked.txt"), "goodbye").unwrap();

        let diff = SystemGit.diff(&dir, Path::new("tracked.txt")).unwrap();
        assert!(diff.contains("-hello"), "diff was: {diff}");
        assert!(diff.contains("+goodbye"), "diff was: {diff}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_reports_an_untracked_file_as_wholly_added() {
        let dir = scratch_git_repo("diff-untracked");
        std::fs::write(dir.join("untracked.txt"), "new content").unwrap();

        let diff = SystemGit.diff(&dir, Path::new("untracked.txt")).unwrap();
        assert!(diff.contains("+new content"), "diff was: {diff}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_reports_a_staged_new_file_in_a_repo_with_no_commits_yet() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-diff-unborn-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "--quiet"]);
        std::fs::write(dir.join("file.txt"), "brand new").unwrap();
        run(&["add", "file.txt"]);

        let diff = SystemGit.diff(&dir, Path::new("file.txt")).unwrap();
        assert!(diff.contains("+brand new"), "diff was: {diff}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_is_empty_for_an_unmodified_tracked_file() {
        let dir = scratch_git_repo("diff-unmodified");

        assert_eq!(SystemGit.diff(&dir, Path::new("tracked.txt")).unwrap(), "");

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
    fn system_git_push_sends_new_commits_to_the_configured_upstream() {
        let remote_dir = std::env::temp_dir().join(format!(
            "syscalls-git-push-remote-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&remote_dir).unwrap();
        assert!(
            Command::new("git")
                .current_dir(&remote_dir)
                .args(["init", "--quiet", "--bare"])
                .status()
                .unwrap()
                .success()
        );

        let dir = scratch_git_repo("push-success");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["remote", "add", "origin", remote_dir.to_str().unwrap()]);
        let branch = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["symbolic-ref", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        // Establishes the upstream tracking branch as fixture setup — the
        // second, unadorned push below is the actual call under test.
        run(&["push", "--quiet", "-u", "origin", &branch]);

        std::fs::write(dir.join("second.txt"), "more").unwrap();
        run(&["add", "second.txt"]);
        run(&["commit", "--quiet", "-m", "second"]);
        let local_head = String::from_utf8(
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

        SystemGit.push(&dir).unwrap();

        let remote_head = String::from_utf8(
            Command::new("git")
                .current_dir(&remote_dir)
                .args(["rev-parse", &branch])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(remote_head, local_head);

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&remote_dir).unwrap();
    }

    #[test]
    fn system_git_push_without_an_upstream_reports_command_failed() {
        let dir = scratch_git_repo("push-no-upstream");

        let err = SystemGit.push(&dir).unwrap_err();
        assert!(matches!(err, PushError::CommandFailed { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Sets up `dir` with an upstream tracking branch established (same
    /// fixture shape as `system_git_push_sends_new_commits_to_the_configured_upstream`),
    /// returning `dir` and the remote's directory for the caller to add
    /// its own local-only commits on top of.
    fn scratch_git_repo_with_upstream(name: &str) -> (PathBuf, PathBuf) {
        let remote_dir = std::env::temp_dir().join(format!(
            "syscalls-git-{name}-remote-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&remote_dir).unwrap();
        assert!(
            Command::new("git")
                .current_dir(&remote_dir)
                .args(["init", "--quiet", "--bare"])
                .status()
                .unwrap()
                .success()
        );

        let dir = scratch_git_repo(name);
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["remote", "add", "origin", remote_dir.to_str().unwrap()]);
        let branch = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["symbolic-ref", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        run(&["push", "--quiet", "-u", "origin", &branch]);

        (dir, remote_dir)
    }

    #[test]
    fn system_git_unpushed_commits_lists_local_only_commits_newest_first() {
        let (dir, remote_dir) = scratch_git_repo_with_upstream("unpushed");
        let run = |args: &[&str]| {
            let status = Command::new("git").current_dir(&dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };

        std::fs::write(dir.join("second.txt"), "more").unwrap();
        run(&["add", "second.txt"]);
        run(&["commit", "--quiet", "-m", "second"]);
        std::fs::write(dir.join("third.txt"), "more still").unwrap();
        run(&["add", "third.txt"]);
        run(&["commit", "--quiet", "-m", "third"]);

        let commits = SystemGit.unpushed_commits(&dir).unwrap();
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["third", "second"]);

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&remote_dir).unwrap();
    }

    #[test]
    fn system_git_unpushed_commits_is_empty_once_up_to_date_with_the_upstream() {
        let (dir, remote_dir) = scratch_git_repo_with_upstream("unpushed-up-to-date");

        assert_eq!(SystemGit.unpushed_commits(&dir).unwrap(), Vec::new());

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&remote_dir).unwrap();
    }

    #[test]
    fn system_git_unpushed_commits_without_an_upstream_reports_command_failed() {
        let dir = scratch_git_repo("unpushed-no-upstream");

        let err = SystemGit.unpushed_commits(&dir).unwrap_err();
        assert!(matches!(err, UnpushedCommitsError::CommandFailed { .. }));

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

    #[test]
    fn fault_injecting_git_overrides_changed_paths() {
        let dir = scratch_git_repo("fault-changed-paths");

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&dir, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.changed_paths(&dir).unwrap_err(),
            ChangedPathsError::Spawn { .. }
        ));

        git.clear(&dir);
        assert!(git.changed_paths(&dir).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_commit_all() {
        let dir = scratch_git_repo("fault-commit-all");
        std::fs::write(dir.join("tracked.txt"), "changed").unwrap();

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&dir, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.commit_all(&dir, "message").unwrap_err(),
            CommitAllError::Spawn { .. }
        ));

        git.clear(&dir);
        assert!(git.commit_all(&dir, "message").is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_diff() {
        let dir = scratch_git_repo("fault-diff");
        let path = dir.join("tracked.txt");
        std::fs::write(&path, "changed").unwrap();

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&path, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.diff(&dir, &path).unwrap_err(),
            DiffError::Spawn { .. }
        ));

        git.clear(&path);
        assert!(git.diff(&dir, &path).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_push() {
        let dir = scratch_git_repo("fault-push");

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&dir, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.push(&dir).unwrap_err(),
            PushError::Spawn { .. }
        ));

        // A real `push` needs a configured upstream remote, which
        // `scratch_git_repo` deliberately doesn't set up — add one so
        // `git.clear` + push also exercises the delegate-to-`inner` path,
        // not just the fault path above.
        let remote_dir = std::env::temp_dir().join(format!(
            "syscalls-git-fault-push-remote-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&remote_dir).unwrap();
        assert!(
            Command::new("git")
                .current_dir(&remote_dir)
                .args(["init", "--quiet", "--bare"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(&dir)
                .args(["remote", "add", "origin", remote_dir.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let branch = String::from_utf8(
            Command::new("git")
                .current_dir(&dir)
                .args(["symbolic-ref", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        // Establishes the upstream tracking branch as fixture setup, via a
        // raw `git push` rather than the wrapper under test — the
        // unadorned `git.push(&dir)` below (relying on that tracking
        // branch, same as `Git::push`'s own contract) is the actual call
        // under test.
        assert!(
            Command::new("git")
                .current_dir(&dir)
                .args(["push", "--quiet", "-u", "origin", &branch])
                .status()
                .unwrap()
                .success()
        );

        git.clear(&dir);
        assert!(git.push(&dir).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&remote_dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_unpushed_commits() {
        let (dir, remote_dir) = scratch_git_repo_with_upstream("fault-unpushed");

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&dir, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.unpushed_commits(&dir).unwrap_err(),
            UnpushedCommitsError::Spawn { .. }
        ));

        git.clear(&dir);
        assert!(git.unpushed_commits(&dir).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&remote_dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_is_repository() {
        let dir = scratch_git_repo("fault-is-repository");

        let mut git = FaultInjectingGit::new(SystemGit);
        assert!(git.is_repository(&dir));

        git.inject(&dir, io::ErrorKind::PermissionDenied);
        assert!(!git.is_repository(&dir));

        git.clear(&dir);
        assert!(git.is_repository(&dir));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fault_injecting_git_overrides_init_repository() {
        let dir = std::env::temp_dir().join(format!(
            "syscalls-git-fault-init-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let mut git = FaultInjectingGit::new(SystemGit);
        git.inject(&dir, io::ErrorKind::PermissionDenied);
        assert!(matches!(
            git.init_repository(&dir).unwrap_err(),
            InitRepositoryError::Spawn { .. }
        ));

        git.clear(&dir);
        assert!(git.init_repository(&dir).is_ok());

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
