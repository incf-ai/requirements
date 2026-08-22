use std::io;
use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFile {
    /// Path relative to the `attachments/` (or `template/`) directory.
    pub path: PathBuf,
    pub content: Vec<u8>,
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
}

/// Reads every file under `dir`, recursively, as an `AttachmentFile` with a
/// path relative to `dir`. `dir` itself must exist. Returns entries sorted by
/// path for deterministic ordering.
pub(crate) fn read_attachments(
    fs: &dyn Filesystem,
    dir: &Path,
) -> Result<Vec<AttachmentFile>, ReadAttachmentsError> {
    if !fs.exists(dir) {
        return Err(ReadAttachmentsError::Missing {
            path: dir.to_path_buf(),
        });
    }

    let mut files = Vec::new();
    read_attachments_into(fs, dir, dir, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn read_attachments_into(
    fs: &dyn Filesystem,
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
            read_attachments_into(fs, root, &entry, out)?;
        } else {
            let content = fs.read(&entry).map_err(|source| ReadAttachmentsError::Io {
                path: entry.clone(),
                source,
            })?;
            let path = entry
                .strip_prefix(root)
                .expect("attachment entry is under its own root")
                .to_path_buf();
            out.push(AttachmentFile { path, content });
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
}

/// Writes each attachment under `dir`, relative to `dir`, creating parent
/// directories as needed. Ensures `dir` itself exists even if `attachments`
/// is empty.
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

    for attachment in attachments {
        let full_path = dir.join(&attachment.path);
        // Always `Some`: `create_dir_all(dir)` above already succeeded,
        // which is only possible if `dir` is non-empty, so `full_path`
        // (which starts with `dir`) always has at least one component to
        // strip.
        let parent = full_path
            .parent()
            .expect("full_path always has a parent: create_dir_all(dir) above already succeeded");
        fs.create_dir_all(parent)
            .map_err(|source| WriteAttachmentsError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        fs.write(&full_path, &attachment.content)
            .map_err(|source| WriteAttachmentsError::Io {
                path: full_path.clone(),
                source,
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use syscalls::{FaultInjectingFilesystem, StdFilesystem};

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
        let err = read_attachments(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Missing { .. }));
    }

    #[test]
    fn read_attachments_reports_not_found_during_the_walk() {
        let dir = temp_dir("read-walk-not-found");
        std::fs::create_dir_all(&dir).unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&dir, io::ErrorKind::NotFound);

        let err = read_attachments(&fs, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Missing { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_reports_generic_io_errors_during_the_walk() {
        let dir = temp_dir("read-walk-io");
        std::fs::create_dir_all(&dir).unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&dir, io::ErrorKind::PermissionDenied);

        let err = read_attachments(&fs, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Io { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_reports_io_errors_reading_a_file() {
        let dir = temp_dir("read-file-io");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, b"hello").unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&file, io::ErrorKind::PermissionDenied);

        let err = read_attachments(&fs, &dir).unwrap_err();
        assert!(matches!(err, ReadAttachmentsError::Io { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attachments_recurses_into_nested_directories() {
        let dir = temp_dir("read-nested");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/inner.txt"), b"hello").unwrap();

        let files = read_attachments(&StdFilesystem, &dir).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, Path::new("nested/inner.txt"));
        assert_eq!(files[0].content, b"hello");

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
    fn write_attachments_reports_io_errors_creating_a_nested_parent() {
        let dir = temp_dir("write-nested-parent-io");
        let nested_parent = dir.join("nested");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&nested_parent, io::ErrorKind::PermissionDenied);

        let attachments = [AttachmentFile {
            path: PathBuf::from("nested/inner.txt"),
            content: b"hello".to_vec(),
        }];
        let err = write_attachments(&fs, &dir, &attachments).unwrap_err();
        assert!(matches!(err, WriteAttachmentsError::Io { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_attachments_reports_io_errors_writing_a_file() {
        let dir = temp_dir("write-file-io");
        let file = dir.join("hello.txt");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&file, io::ErrorKind::PermissionDenied);

        let attachments = [AttachmentFile {
            path: PathBuf::from("hello.txt"),
            content: b"hello".to_vec(),
        }];
        let err = write_attachments(&fs, &dir, &attachments).unwrap_err();
        assert!(matches!(err, WriteAttachmentsError::Io { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }
}
