use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use syscalls::Filesystem;
use thiserror::Error;

pub(crate) fn ron_options() -> ron::Options {
    ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::EXPLICIT_STRUCT_NAMES)
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .with_default_extension(ron::extensions::Extensions::UNWRAP_NEWTYPES)
        .with_default_extension(ron::extensions::Extensions::UNWRAP_VARIANT_NEWTYPES)
}

#[derive(Debug, Error)]
pub(crate) enum LoadRonError {
    #[error("missing required file: {path}")]
    Missing { path: PathBuf },
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse RON at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: ron::de::SpannedError,
    },
}

pub(crate) fn load_ron<T: DeserializeOwned>(
    fs: &dyn Filesystem,
    path: &Path,
) -> Result<T, LoadRonError> {
    let contents = match fs.read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(LoadRonError::Missing {
                path: path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(LoadRonError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    ron_options()
        .from_str(&contents)
        .map_err(|source| LoadRonError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

#[derive(Debug, Error)]
pub(crate) enum SaveRonError {
    #[error("failed to serialize RON for {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: ron::Error,
    },
    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub(crate) fn save_ron<T: Serialize>(
    fs: &dyn Filesystem,
    path: &Path,
    value: &T,
) -> Result<(), SaveRonError> {
    let contents = ron_options()
        .to_string_pretty(value, ron::ser::PrettyConfig::default())
        .map_err(|source| SaveRonError::Serialize {
            path: path.to_path_buf(),
            source,
        })?;
    fs.write(path, contents.as_bytes())
        .map_err(|source| SaveRonError::Io {
            path: path.to_path_buf(),
            source,
        })
}

#[derive(Debug, Error)]
pub(crate) enum ReadRequiredTextError {
    #[error("missing required file: {path}")]
    Missing { path: PathBuf },
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Reads a sibling text file that must always exist (its content may still be empty).
pub(crate) fn read_required_text(
    fs: &dyn Filesystem,
    path: &Path,
) -> Result<String, ReadRequiredTextError> {
    match fs.read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(ReadRequiredTextError::Missing {
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(ReadRequiredTextError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Debug, Error)]
pub(crate) enum ReadOptionalTextError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Reads a sibling text file that may be entirely absent.
pub(crate) fn read_optional_text(
    fs: &dyn Filesystem,
    path: &Path,
) -> Result<Option<String>, ReadOptionalTextError> {
    match fs.read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ReadOptionalTextError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Debug, Error)]
pub(crate) enum WriteTextError {
    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub(crate) fn write_text(
    fs: &dyn Filesystem,
    path: &Path,
    contents: &str,
) -> Result<(), WriteTextError> {
    fs.write(path, contents.as_bytes())
        .map_err(|source| WriteTextError::Io {
            path: path.to_path_buf(),
            source,
        })
}

pub(crate) fn write_optional_text(
    fs: &dyn Filesystem,
    path: &Path,
    contents: Option<&str>,
) -> Result<(), WriteTextError> {
    match contents {
        Some(contents) => write_text(fs, path, contents),
        None => Ok(()),
    }
}

/// The directory name of one child entry inside a `requirements/`, `tests/`,
/// `results/`, or `modules/` folder — a single path component (never a
/// multi-segment path), used as the key identifying that child.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryName(pub String);

impl EntryName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `EntryName` of `dir` itself, i.e. `dir`'s own final path component.
    pub(crate) fn of(dir: &Path) -> Self {
        EntryName(
            dir.file_name()
                .expect("directory has a file name")
                .to_string_lossy()
                .into_owned(),
        )
    }
}

impl std::fmt::Display for EntryName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<Path> for EntryName {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

#[derive(Debug, Error)]
pub(crate) enum LoadNamedChildrenError<E: std::error::Error + 'static> {
    #[error("missing required directory: {path}")]
    Missing { path: PathBuf },
    #[error("io error reading directory {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to load '{name}': {source}")]
    Child {
        name: EntryName,
        #[source]
        source: E,
    },
}

/// Lists the immediate subdirectories of `dir`, sorted by name, each loaded
/// with `loader` (which is expected to embed its own `EntryName` in the
/// value it returns). `dir` itself must exist.
pub(crate) fn load_named_children<T, E: std::error::Error + 'static>(
    fs: &dyn Filesystem,
    dir: &Path,
    loader: impl Fn(&dyn Filesystem, &Path) -> Result<T, E>,
) -> Result<Vec<T>, LoadNamedChildrenError<E>> {
    let mut entries = match fs.read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(LoadNamedChildrenError::Missing {
                path: dir.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(LoadNamedChildrenError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    entries.sort();

    entries
        .into_iter()
        .filter(|entry| fs.is_dir(entry))
        .map(|entry| {
            loader(fs, &entry).map_err(|source| LoadNamedChildrenError::Child {
                name: EntryName::of(&entry),
                source,
            })
        })
        .collect()
}
