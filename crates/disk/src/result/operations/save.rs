use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

use crate::attachments::{WriteAttachmentsError, write_attachments};
use crate::result::types::{ResultDefinition, ResultOnDisk};
use crate::util::{SaveRonError, save_ron};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to save result.ron: {0}")]
    Definition(#[from] SaveRonError),
    #[error("failed to save attachments: {0}")]
    Attachments(#[from] WriteAttachmentsError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn save_result(fs: &dyn Filesystem, dir: &Path, result: &ResultOnDisk) -> Result<(), Error> {
    save_result_inner(fs, dir, result).map_err(Error)
}

fn save_result_inner(
    fs: &dyn Filesystem,
    dir: &Path,
    result: &ResultOnDisk,
) -> Result<(), ErrorKind> {
    fs.create_dir_all(dir)
        .map_err(|source| ErrorKind::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;

    save_ron(
        fs,
        &dir.join("result.ron"),
        &ResultDefinition::ResultsV1(result.definition.clone()),
    )?;
    write_attachments(fs, &dir.join("attachments"), &result.attachments)?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::result::operations::load::load_result;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn round_trips_a_result_through_a_tempdir() -> Result<(), Box<dyn std::error::Error>> {
        let dir = sample_project_dir().join("results/definition");
        let original = load_result(&StdFilesystem, &FixedGit, &dir)?;

        let tempdir = std::env::temp_dir().join(format!(
            "disk-result-round-trip-{}-{}",
            std::process::id(),
            line!()
        ));
        save_result(&StdFilesystem, &tempdir, &original)?;
        let reloaded = load_result(&StdFilesystem, &FixedGit, &tempdir)?;

        assert_eq!(original.definition.title, reloaded.definition.title);
        assert_eq!(original.definition.commit, reloaded.definition.commit);
        assert_eq!(original.attachments, reloaded.attachments);

        std::fs::remove_dir_all(&tempdir).ok();
        Ok(())
    }

    fn minimal_result() -> ResultOnDisk {
        ResultOnDisk {
            name: crate::util::EntryName("definition".to_string()),
            definition: crate::result::types::ResultsV1 {
                title: "Title".to_string(),
                path: crate::requirement::types::ReferencePath(
                    "requirements/definition".to_string(),
                ),
                commit: "abc".to_string(),
                status: crate::result::types::StatusV1::default(),
                attachment: None,
                attachments: None,
            },
            attachments: Vec::new(),
        }
    }

    #[test]
    fn reports_io_errors_saving_result_ron() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-result-save-definition-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("result.ron"), std::io::ErrorKind::PermissionDenied);

        let err = save_result(&fs, &dir, &minimal_result()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Definition(_)));

        std::fs::remove_dir_all(&dir).ok();
    }
}
