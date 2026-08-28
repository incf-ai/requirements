use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use syscalls::{CommitForPathError, Filesystem, Git};
use thiserror::Error;

use crate::util::EntryName;

/// Git doesn't track empty directories, so an `attachments/`-style folder
/// with no real attachments would silently vanish from the repository.
/// `write_attachments` always drops a placeholder file with this name into
/// the directory to keep it present in git regardless; `read_attachments`
/// ignores it, so it never shows up as an `AttachmentFile`.
const PLACEHOLDER_FILENAME: &str = ".gitkeep";

/// A reference to an attachment file: where it lives and the git commit that
/// last touched it. The raw bytes are never stored here — the file itself
/// lives on disk (and in git history), this just records path + commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFile {
    /// Path relative to the `attachments/` (or `template/`) directory.
    pub path: PathBuf,
    /// The full git commit hash that last touched this file.
    pub commit: String,
}

/// A `requirement.ron`/`test.ron`/`result.ron` reference to an attachment
/// file, either physically local to the referencing entity or shared at the
/// module level (as opposed to `AttachmentFile`, which is a file `disk` has
/// actually walked and resolved a commit for). Resolving this against the
/// actual attachment list is out of scope for `disk` — it only carries
/// `name`/`path` as parsed from RON.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum AttachmentReferenceKind {
    /// A file in this entity's own local `attachments/` folder.
    LocalAttachmentReferenceV1 {
        /// A logical/display name for this reference, independent of
        /// `path` — same relationship as `RequirementOnDisk::name` (a
        /// directory name) to `RequirementDefinitionV1::title` (freeform
        /// text): the two are allowed to differ.
        name: EntryName,
        /// Where the file actually is, relative to the `attachments/`
        /// directory (supports nested paths, unlike `name`).
        path: PathBuf,
    },
    /// A file in this entity's module's shared `attachments/` folder.
    ModuleAttachmentReferenceV1 {
        /// See `LocalAttachmentReferenceV1::name`.
        name: EntryName,
        /// Where the file actually is, relative to the module's
        /// `attachments/` directory (supports nested paths, unlike `name`).
        path: PathBuf,
    },
}

#[derive(Debug, Error)]
pub(crate) enum ReadAttachmentsError {
    #[error("missing required directory: {path}")]
    Missing { path: PathBuf },
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to look up commit for {path}: {source}")]
    Commit {
        path: PathBuf,
        #[source]
        source: CommitForPathError,
    },
}

/// Reads every file under `dir`, recursively, as an `AttachmentFile` with a
/// path relative to `dir` and the commit `git` reports for it. `dir` itself
/// must exist. Returns entries sorted by path for deterministic ordering.
pub(crate) fn read_attachments(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &Path,
) -> Result<Vec<AttachmentFile>, ReadAttachmentsError> {
    if !fs.exists(dir) {
        return Err(ReadAttachmentsError::Missing {
            path: dir.to_path_buf(),
        });
    }

    let mut files = Vec::new();
    read_attachments_into(fs, git, dir, dir, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn read_attachments_into(
    fs: &dyn Filesystem,
    git: &dyn Git,
    root: &Path,
    current: &Path,
    out: &mut Vec<AttachmentFile>,
) -> Result<(), ReadAttachmentsError> {
    let entries = match fs.read_dir(current) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ReadAttachmentsError::Missing {
                path: current.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(ReadAttachmentsError::Io {
                path: current.to_path_buf(),
                source,
            });
        }
    };

    for entry in entries {
        if fs.is_dir(&entry) {
            read_attachments_into(fs, git, root, &entry, out)?;
        } else if entry.file_name() == Some(std::ffi::OsStr::new(PLACEHOLDER_FILENAME)) {
            continue;
        } else {
            let commit =
                git.commit_for_path(&entry)
                    .map_err(|source| ReadAttachmentsError::Commit {
                        path: entry.clone(),
                        source,
                    })?;
            let path = entry
                .strip_prefix(root)
                .expect("attachment entry is under its own root")
                .to_path_buf();
            out.push(AttachmentFile { path, commit });
        }
    }

    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum WriteAttachmentsError {
    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("referenced attachment does not exist on disk: {path}")]
    Missing { path: PathBuf },
}

/// Ensures `dir` exists and that every attachment's file is already present
/// under it. The `disk` crate never writes attachment bytes itself — the
/// files are expected to already be on disk (and committed to git) by other
/// means; this only validates that each reference still resolves to a file.
/// Also always drops a `.gitkeep` placeholder in `dir` (see
/// `PLACEHOLDER_FILENAME`) so the directory stays present in git even when
/// `attachments` is empty.
pub(crate) fn write_attachments(
    fs: &dyn Filesystem,
    dir: &Path,
    attachments: &[AttachmentFile],
) -> Result<(), WriteAttachmentsError> {
    fs.create_dir_all(dir)
        .map_err(|source| WriteAttachmentsError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

    let placeholder = dir.join(PLACEHOLDER_FILENAME);
    fs.write(&placeholder, b"")
        .map_err(|source| WriteAttachmentsError::Io {
            path: placeholder,
            source,
        })?;

    for attachment in attachments {
        let full_path = dir.join(&attachment.path);
        if !fs.exists(&full_path) {
            return Err(WriteAttachmentsError::Missing { path: full_path });
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_support::FixedGit;
    use syscalls::{FaultInjectingFilesystem, FaultInjectingGit, StdFilesystem};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "disk-attachments-{name}-{}-{}",
            std::process::id(),
            line!()
        ))
    }

    #[test]
    fn read_attachments_reports_missing_directory() {
        let dir = temp_dir("read-missing");
        let err = read_attachments(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Missing { .. }));
    }

    #[test]
    fn read_attachments_reports_not_found_during_the_walk() {
        let dir = temp_dir("read-walk-not-found");
        std::fs::create_dir_all(&dir).unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&dir, io::ErrorKind::NotFound);

        let err = read_attachments(&fs, &FixedGit, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Missing { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_reports_generic_io_errors_during_the_walk() {
        let dir = temp_dir("read-walk-io");
        std::fs::create_dir_all(&dir).unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&dir, io::ErrorKind::PermissionDenied);

        let err = read_attachments(&fs, &FixedGit, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Io { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_reports_commit_lookup_errors() {
        let dir = temp_dir("read-file-commit");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, b"hello").unwrap();

        let mut git = FaultInjectingGit::new(FixedGit);
        git.inject(&file, io::ErrorKind::PermissionDenied);

        let err = read_attachments(&StdFilesystem, &git, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Commit { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_recurses_into_nested_directories() {
        let dir = temp_dir("read-nested");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/inner.txt"), b"hello").unwrap();

        let files = read_attachments(&StdFilesystem, &FixedGit, &dir).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, Path::new("nested/inner.txt"));
        assert_eq!(files[0].commit, "deadbeef");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_propagates_an_error_from_a_nested_directory() {
        // `read_attachments_reports_generic_io_errors_during_the_walk`
        // above only exercises the top-level `read_dir` failing; the
        // recursive call at the walk's `read_attachments_into(..)?` for a
        // *nested* directory is a separate code path.
        let dir = temp_dir("read-nested-io");
        std::fs::create_dir_all(dir.join("nested")).unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("nested"), io::ErrorKind::PermissionDenied);

        let err = read_attachments(&fs, &FixedGit, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Io { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_attachments_reports_io_errors_creating_the_directory() {
        let dir = temp_dir("write-create-dir-io");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&dir, io::ErrorKind::PermissionDenied);

        let err = write_attachments(&fs, &dir, &[]).unwrap_err();
        assert!(matches!(err, WriteAttachmentsError::Io { .. }));
    }

    #[test]
    fn write_attachments_reports_io_errors_writing_the_placeholder() {
        let dir = temp_dir("write-placeholder-io");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join(PLACEHOLDER_FILENAME),
            io::ErrorKind::PermissionDenied,
        );

        let err = write_attachments(&fs, &dir, &[]).unwrap_err();
        assert!(matches!(err, WriteAttachmentsError::Io { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_attachments_leaves_a_placeholder_when_empty() {
        let dir = temp_dir("write-placeholder");

        write_attachments(&StdFilesystem, &dir, &[]).unwrap();
        assert!(dir.join(PLACEHOLDER_FILENAME).exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_ignores_the_placeholder() {
        let dir = temp_dir("read-ignores-placeholder");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PLACEHOLDER_FILENAME), b"").unwrap();

        let files = read_attachments(&StdFilesystem, &FixedGit, &dir).unwrap();
        assert!(files.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_attachments_reports_missing_referenced_files() {
        let dir = temp_dir("write-missing-file");

        let attachments = [AttachmentFile {
            path: PathBuf::from("nested/inner.txt"),
            commit: "deadbeef".to_string(),
        }];
        let err = write_attachments(&StdFilesystem, &dir, &attachments).unwrap_err();
        assert!(matches!(err, WriteAttachmentsError::Missing { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_attachments_accepts_attachments_already_present_on_disk() {
        let dir = temp_dir("write-present-file");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/inner.txt"), b"hello").unwrap();

        let attachments = [AttachmentFile {
            path: PathBuf::from("nested/inner.txt"),
            commit: "deadbeef".to_string(),
        }];
        write_attachments(&StdFilesystem, &dir, &attachments).unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }
}
