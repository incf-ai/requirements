use std::path::{Component, Path, PathBuf};

use disk::EntryName;
use thiserror::Error;

/// See `crates/logical/README.md`, "Validation questions — answered" #5:
/// names/paths are sanitized at `add_*` call time, not deferred to
/// `validate()` or left to `disk` to reject at save time.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvalidNameError {
    #[error("name must not be empty")]
    Empty,
    #[error("name `{0}` must not contain `/`")]
    ContainsSlash(String),
    #[error("name `{0}` must not be `.` or `..`")]
    DotOrDotDot(String),
    #[error("name `{0}` must not have leading/trailing whitespace")]
    Whitespace(String),
}

/// Validates a single directory-name component (a requirement stage, test,
/// result, or submodule name) before it's ever handed to `disk`, which
/// would otherwise `Path::join` it verbatim.
pub(crate) fn sanitize_entry_name(name: &str) -> Result<EntryName, InvalidNameError> {
    if name.is_empty() {
        return Err(InvalidNameError::Empty);
    }
    if name.contains('/') {
        return Err(InvalidNameError::ContainsSlash(name.to_string()));
    }
    if name == "." || name == ".." {
        return Err(InvalidNameError::DotOrDotDot(name.to_string()));
    }
    if name.trim() != name {
        return Err(InvalidNameError::Whitespace(name.to_string()));
    }
    Ok(EntryName(name.to_string()))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvalidPathError {
    #[error("path must not be empty")]
    Empty,
    #[error("path `{}` must be relative", .0.display())]
    Absolute(PathBuf),
    #[error("path `{}` must not contain a `..` component", .0.display())]
    ParentComponent(PathBuf),
}

/// Validates an attachment/template path (relative to whichever pool it's
/// being added to) before it's ever handed to `disk`.
pub(crate) fn sanitize_relative_path(path: &Path) -> Result<PathBuf, InvalidPathError> {
    if path.as_os_str().is_empty() {
        return Err(InvalidPathError::Empty);
    }
    if path.is_absolute() {
        return Err(InvalidPathError::Absolute(path.to_path_buf()));
    }
    // `is_absolute()` above already rules out a `RootDir`/`Prefix` component
    // ever appearing here (on every platform `disk`/`logical` target), so
    // there's nothing left to check per-component except `..`.
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(InvalidPathError::ParentComponent(path.to_path_buf()));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn accepts_a_plain_name() {
        assert_eq!(
            sanitize_entry_name("definition").unwrap(),
            EntryName("definition".to_string())
        );
    }

    #[test]
    fn rejects_an_empty_name() {
        assert_eq!(
            sanitize_entry_name("").unwrap_err(),
            InvalidNameError::Empty
        );
    }

    #[test]
    fn rejects_a_name_containing_a_slash() {
        assert!(matches!(
            sanitize_entry_name("a/b").unwrap_err(),
            InvalidNameError::ContainsSlash(_)
        ));
    }

    #[test]
    fn rejects_dot() {
        assert!(matches!(
            sanitize_entry_name(".").unwrap_err(),
            InvalidNameError::DotOrDotDot(_)
        ));
    }

    #[test]
    fn rejects_dot_dot() {
        assert!(matches!(
            sanitize_entry_name("..").unwrap_err(),
            InvalidNameError::DotOrDotDot(_)
        ));
    }

    #[test]
    fn rejects_leading_or_trailing_whitespace() {
        assert!(matches!(
            sanitize_entry_name(" definition").unwrap_err(),
            InvalidNameError::Whitespace(_)
        ));
        assert!(matches!(
            sanitize_entry_name("definition ").unwrap_err(),
            InvalidNameError::Whitespace(_)
        ));
    }

    #[test]
    fn accepts_a_plain_relative_path() {
        assert_eq!(
            sanitize_relative_path(Path::new("logo.png")).unwrap(),
            PathBuf::from("logo.png")
        );
    }

    #[test]
    fn accepts_a_nested_relative_path() {
        assert_eq!(
            sanitize_relative_path(Path::new("nested/logo.png")).unwrap(),
            PathBuf::from("nested/logo.png")
        );
    }

    #[test]
    fn rejects_an_empty_path() {
        assert_eq!(
            sanitize_relative_path(Path::new("")).unwrap_err(),
            InvalidPathError::Empty
        );
    }

    #[test]
    fn rejects_an_absolute_path() {
        assert!(matches!(
            sanitize_relative_path(Path::new("/etc/passwd")).unwrap_err(),
            InvalidPathError::Absolute(_)
        ));
    }

    #[test]
    fn rejects_a_parent_dir_component() {
        assert!(matches!(
            sanitize_relative_path(Path::new("../secret")).unwrap_err(),
            InvalidPathError::ParentComponent(_)
        ));
    }

    #[test]
    fn accepts_a_leading_current_dir_component() {
        assert_eq!(
            sanitize_relative_path(Path::new("./logo.png")).unwrap(),
            PathBuf::from("./logo.png")
        );
    }
}
