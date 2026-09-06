use std::path::{Component, Path, PathBuf};

use disk::EntryName;
use thiserror::Error;

/// See `crates/logical/README.md`, "Validation questions — answered" #5:
/// names/paths are sanitized at `add_*` call time, not deferred to
/// `validate()` or left to `disk` to reject at save time. Rejects anything
/// that would be an invalid filename on Windows as well as Unix, since a
/// project's directory tree may be cloned/opened on either — see
/// `sanitize_entry_name`'s own doc comment for the specific rules.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvalidNameError {
    #[error("name must not be empty")]
    Empty,
    #[error("name `{0}` must not contain `/`")]
    ContainsSlash(String),
    #[error("name `{0}` must not contain `\\`")]
    ContainsBackslash(String),
    #[error("name `{0}` must not contain the reserved character `{1}`")]
    ContainsReservedChar(String, char),
    #[error("name `{0}` must not contain control characters")]
    ContainsControlChar(String),
    #[error("name `{0}` must not be `.` or `..`")]
    DotOrDotDot(String),
    #[error("name `{0}` must not have leading/trailing whitespace")]
    Whitespace(String),
    #[error("name `{0}` must not end with `.` or ` ` (not preserved on Windows)")]
    TrailingDotOrSpace(String),
    #[error("name `{0}` is a reserved device name on Windows")]
    ReservedDeviceName(String),
}

/// Characters Windows forbids in a filename, beyond `/` and `\\` (each
/// checked separately above so their errors can name the offending
/// character precisely — `/` doubles as the Unix path separator and `\\`
/// as an easy source of confusion when a Windows-style path is pasted in).
const WINDOWS_RESERVED_CHARS: [char; 6] = ['<', '>', ':', '"', '|', '?'];

/// Windows' reserved device names — forbidden as a filename regardless of
/// case or extension (e.g. `NUL`, `nul.txt`). `COM`/`LPT` are reserved for
/// digits 1-9 (not 0).
const WINDOWS_RESERVED_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validates a single directory-name component (a requirement stage, test,
/// result, or submodule name) before it's ever handed to `disk`, which
/// would otherwise `Path::join` it verbatim. Beyond what's illegal on Unix
/// (empty, `/`, `.`/`..`), this also rejects anything that isn't a valid
/// Windows filename — `\`, the reserved characters `< > : " | ? *`, ASCII
/// control characters, a reserved device name (`CON`, `NUL`, `COM1`, ...),
/// and a trailing `.` or ` ` (Windows silently strips these, so `"foo."`
/// and `"foo"` would collide) — so the same project tree stays usable
/// whether it's cloned on Unix or Windows.
pub(crate) fn sanitize_entry_name(name: &str) -> Result<EntryName, InvalidNameError> {
    if name.is_empty() {
        return Err(InvalidNameError::Empty);
    }
    if name.contains('/') {
        return Err(InvalidNameError::ContainsSlash(name.to_string()));
    }
    if name.contains('\\') {
        return Err(InvalidNameError::ContainsBackslash(name.to_string()));
    }
    if let Some(c) = name.chars().find(|c| WINDOWS_RESERVED_CHARS.contains(c) || *c == '*') {
        return Err(InvalidNameError::ContainsReservedChar(name.to_string(), c));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(InvalidNameError::ContainsControlChar(name.to_string()));
    }
    if name == "." || name == ".." {
        return Err(InvalidNameError::DotOrDotDot(name.to_string()));
    }
    // Checked before the general whitespace trim below so a trailing space
    // reports as this more specific (Windows-collision) reason rather than
    // the generic `Whitespace` one.
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(InvalidNameError::TrailingDotOrSpace(name.to_string()));
    }
    if name.trim() != name {
        return Err(InvalidNameError::Whitespace(name.to_string()));
    }
    let stem = name.split('.').next().unwrap_or(name);
    if WINDOWS_RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
    {
        return Err(InvalidNameError::ReservedDeviceName(name.to_string()));
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
    fn rejects_leading_whitespace() {
        assert!(matches!(
            sanitize_entry_name(" definition").unwrap_err(),
            InvalidNameError::Whitespace(_)
        ));
    }

    #[test]
    fn rejects_trailing_backslash() {
        assert!(matches!(
            sanitize_entry_name("a\\b").unwrap_err(),
            InvalidNameError::ContainsBackslash(_)
        ));
    }

    #[test]
    fn rejects_windows_reserved_characters() {
        for name in ["a<b", "a>b", "a:b", "a\"b", "a|b", "a?b", "a*b"] {
            assert!(
                matches!(
                    sanitize_entry_name(name).unwrap_err(),
                    InvalidNameError::ContainsReservedChar(_, _)
                ),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_control_characters() {
        assert!(matches!(
            sanitize_entry_name("a\tb").unwrap_err(),
            InvalidNameError::ContainsControlChar(_)
        ));
    }

    #[test]
    fn rejects_trailing_dot_or_space() {
        assert!(matches!(
            sanitize_entry_name("definition.").unwrap_err(),
            InvalidNameError::TrailingDotOrSpace(_)
        ));
        assert!(matches!(
            sanitize_entry_name("definition ").unwrap_err(),
            InvalidNameError::TrailingDotOrSpace(_)
        ));
    }

    #[test]
    fn rejects_windows_reserved_device_names() {
        for name in ["CON", "con", "NUL.txt", "com1", "LPT9"] {
            assert!(
                matches!(
                    sanitize_entry_name(name).unwrap_err(),
                    InvalidNameError::ReservedDeviceName(_)
                ),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_a_name_that_merely_contains_a_reserved_word_as_a_substring() {
        assert!(sanitize_entry_name("CONFIG").is_ok());
        assert!(sanitize_entry_name("economic").is_ok());
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
