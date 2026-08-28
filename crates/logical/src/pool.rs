use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::sanitize::{InvalidPathError, sanitize_relative_path};

/// Shared by every local/module attachment or template pool (a flat
/// `BTreeSet<PathBuf>` — see `crates/logical/README.md`'s data model
/// section for why paths, not names, are the key).
#[derive(Debug, Error)]
pub enum AddPoolFileError {
    #[error("invalid path: {0}")]
    InvalidPath(#[from] InvalidPathError),
    #[error("`{}` is already in the pool", .0.display())]
    AlreadyExists(PathBuf),
}

pub(crate) fn add_pool_file(
    pool: &mut BTreeSet<PathBuf>,
    path: &Path,
) -> Result<(), AddPoolFileError> {
    let path = sanitize_relative_path(path)?;
    if pool.contains(&path) {
        return Err(AddPoolFileError::AlreadyExists(path));
    }
    pool.insert(path);
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn adds_a_new_file() {
        let mut pool = BTreeSet::new();
        add_pool_file(&mut pool, Path::new("logo.png")).unwrap();
        assert!(pool.contains(Path::new("logo.png")));
    }

    #[test]
    fn rejects_an_invalid_path() {
        let mut pool = BTreeSet::new();
        let err = add_pool_file(&mut pool, Path::new("../logo.png")).unwrap_err();
        assert!(matches!(err, AddPoolFileError::InvalidPath(_)));
    }

    #[test]
    fn rejects_a_duplicate_path() {
        let mut pool = BTreeSet::new();
        add_pool_file(&mut pool, Path::new("logo.png")).unwrap();
        let err = add_pool_file(&mut pool, Path::new("logo.png")).unwrap_err();
        assert!(matches!(err, AddPoolFileError::AlreadyExists(_)));
    }
}
