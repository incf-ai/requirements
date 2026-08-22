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

#[cfg(test)]
mod test {
    use super::*;
    use crate::requirement::types::RequirementDefinition;
    use syscalls::{FaultInjectingFilesystem, StdFilesystem};

    /// A `Serialize` type used by both `save_ron` tests below, so both
    /// exercise the same monomorphization of the generic `save_ron` (branch
    /// coverage is tracked per-instantiation, so splitting these across two
    /// different concrete types would leave each instantiation only
    /// half-covered).
    struct MaybeFailingSerialize(bool);

    impl Serialize for MaybeFailingSerialize {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if self.0 {
                Err(serde::ser::Error::custom("boom"))
            } else {
                serializer.serialize_unit()
            }
        }
    }

    #[test]
    fn load_ron_reports_generic_io_errors() {
        // Reuses the `RequirementDefinition` instantiation of `load_ron`
        // (already exercised for `Missing`/`Parse` elsewhere) rather than a
        // one-off type param, so that instantiation ends up fully covered.
        let path = Path::new("/nonexistent/does-not-matter.ron");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(path, io::ErrorKind::PermissionDenied);

        let err = load_ron::<RequirementDefinition>(&fs, path).unwrap_err();
        assert!(matches!(err, LoadRonError::Io { .. }));
    }

    #[test]
    fn save_ron_reports_serialize_errors() {
        let path = Path::new("/nonexistent/does-not-matter.ron");
        let err = save_ron(&StdFilesystem, path, &MaybeFailingSerialize(true)).unwrap_err();
        assert!(matches!(err, SaveRonError::Serialize { .. }));
    }

    #[test]
    fn save_ron_reports_generic_io_errors() {
        let path = Path::new("/nonexistent/does-not-matter.ron");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(path, io::ErrorKind::PermissionDenied);

        let err = save_ron(&fs, path, &MaybeFailingSerialize(false)).unwrap_err();
        assert!(matches!(err, SaveRonError::Io { .. }));
    }

    #[test]
    fn read_required_text_reports_generic_io_errors() {
        let path = Path::new("/nonexistent/does-not-matter.typ");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(path, io::ErrorKind::PermissionDenied);

        let err = read_required_text(&fs, path).unwrap_err();
        assert!(matches!(err, ReadRequiredTextError::Io { .. }));
    }

    #[test]
    fn read_optional_text_reports_generic_io_errors() {
        let path = Path::new("/nonexistent/does-not-matter.typ");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(path, io::ErrorKind::PermissionDenied);

        let err = read_optional_text(&fs, path).unwrap_err();
        assert!(matches!(err, ReadOptionalTextError::Io { .. }));
    }

    #[test]
    fn write_text_reports_generic_io_errors() {
        let path = Path::new("/nonexistent/does-not-matter.typ");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(path, io::ErrorKind::PermissionDenied);

        let err = write_text(&fs, path, "hello").unwrap_err();
        assert!(matches!(err, WriteTextError::Io { .. }));
    }

    #[test]
    fn write_optional_text_skips_writing_when_none() {
        let dir = std::env::temp_dir().join(format!(
            "disk-util-write-optional-none-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("absent.typ");

        write_optional_text(&StdFilesystem, &path, None).unwrap();
        assert!(!path.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_named_children_reports_generic_io_errors() {
        // Reuses `load_requirement_stage` as the loader — the same
        // instantiation `module::operations::load_module_tree` uses for its
        // `requirements/` children — rather than a throwaway closure type,
        // so that instantiation (already covered for `Missing` and, below,
        // `Child`) ends up covering `Io` too.
        let dir = Path::new("/nonexistent/does-not-matter-dir");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir, io::ErrorKind::PermissionDenied);

        let err = load_named_children(&fs, dir, crate::requirement::operations::load_requirement_stage)
            .unwrap_err();
        assert!(matches!(err, LoadNamedChildrenError::Io { .. }));
    }

    #[test]
    fn load_named_children_reports_child_errors() {
        let dir = std::env::temp_dir().join(format!(
            "disk-util-named-children-child-{}-{}",
            std::process::id(),
            line!()
        ));
        // A "requirements/"-shaped directory with one broken child (missing
        // its required `requirement.ron`).
        std::fs::create_dir_all(dir.join("broken")).unwrap();

        let err = load_named_children(
            &StdFilesystem,
            &dir,
            crate::requirement::operations::load_requirement_stage,
        )
        .unwrap_err();

        assert!(matches!(err, LoadNamedChildrenError::Child { .. }));
        assert!(err.to_string().contains("broken"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[should_panic(expected = "directory has a file name")]
    fn entry_name_of_panics_without_a_file_name() {
        EntryName::of(Path::new("/"));
    }

    #[test]
    fn entry_name_display_matches_the_inner_string() {
        assert_eq!(EntryName("definition".to_string()).to_string(), "definition");
    }

    #[test]
    fn load_ron_error_messages_are_readable() {
        let path = PathBuf::from("/some/path.ron");
        let err = LoadRonError::Missing { path: path.clone() };
        assert_eq!(err.to_string(), "missing required file: /some/path.ron");
    }
}
