use std::path::Path;

use syscalls::Filesystem;
use thiserror::Error;

use crate::attachments::{ReadAttachmentsError, read_attachments};
use crate::result::types::{ResultDefinition, ResultOnDisk};
use crate::util::{EntryName, LoadRonError, load_ron};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to load result.ron: {0}")]
    Definition(#[from] LoadRonError),
    #[error("failed to load attachments: {0}")]
    Attachments(#[from] ReadAttachmentsError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn load_result(fs: &dyn Filesystem, dir: &Path) -> Result<ResultOnDisk, Error> {
    load_result_inner(fs, dir).map_err(Error)
}

fn load_result_inner(fs: &dyn Filesystem, dir: &Path) -> Result<ResultOnDisk, ErrorKind> {
    let ResultDefinition::ResultsV1(definition) = load_ron(fs, &dir.join("result.ron"))?;
    let attachments = read_attachments(fs, &dir.join("attachments"))?;

    Ok(ResultOnDisk {
        name: EntryName::of(dir),
        definition,
        attachments,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    /// Builds a minimal valid `results/<stage>/` folder in a fresh tempdir.
    fn valid_result_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "disk-result-load-{name}-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        std::fs::write(
            dir.join("result.ron"),
            "ResultsV1(title: \"Title\", path: \"requirements/definition\", commit: \"abc\")",
        )
        .unwrap();
        dir
    }

    #[test]
    fn loads_the_definition_result_from_the_sample_project() -> Result<(), Error> {
        let dir = sample_project_dir().join("results/definition");
        let result = load_result(&StdFilesystem, &dir)?;

        assert_eq!(result.definition.title, "Definition");
        assert_eq!(result.definition.path.0, "requirements/definition");
        assert!(matches!(result.definition.status, crate::result::types::StatusV1::Incomplete));
        assert!(result.attachments.is_empty());

        Ok(())
    }

    #[test]
    fn missing_result_ron_is_reported() {
        let dir = valid_result_dir("missing-ron");
        std::fs::remove_file(dir.join("result.ron")).unwrap();

        let err = load_result(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
