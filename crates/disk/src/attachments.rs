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
        if let Some(parent) = full_path.parent() {
            fs.create_dir_all(parent)
                .map_err(|source| WriteAttachmentsError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        fs.write(&full_path, &attachment.content)
            .map_err(|source| WriteAttachmentsError::Io {
                path: full_path.clone(),
                source,
            })?;
    }

    Ok(())
}
