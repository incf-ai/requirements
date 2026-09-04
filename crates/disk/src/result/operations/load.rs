use std::path::{Path, PathBuf};

use syscalls::{Filesystem, Git};
use thiserror::Error;

use crate::attachments::{ReadAttachmentsError, read_attachments};
use crate::result::types::{ResultDefinition, ResultOnDisk, ValidateResultsError};
use crate::util::{EntryName, LoadRonError, load_ron};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to load result.ron: {0}")]
    Definition(#[from] LoadRonError),
    #[error("invalid result.ron at {path}: {source}")]
    Invalid {
        path: PathBuf,
        #[source]
        source: ValidateResultsError,
    },
    #[error("failed to load attachments: {0}")]
    Attachments(#[from] ReadAttachmentsError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn load_result(fs: &dyn Filesystem, git: &dyn Git, dir: &Path) -> Result<ResultOnDisk, Error> {
    load_result_inner(fs, git, dir).map_err(Error)
}

fn load_result_inner(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &Path,
) -> Result<ResultOnDisk, ErrorKind> {
    let ron_path = dir.join("result.ron");
    let ResultDefinition::ResultsV1(definition) = load_ron(fs, &ron_path)?;
    definition.validate().map_err(|source| ErrorKind::Invalid {
        path: ron_path.clone(),
        source,
    })?;
    let attachments = read_attachments(fs, git, &dir.join("attachments"))?;

    Ok(ResultOnDisk {
        name: EntryName::of(dir),
        definition,
        attachments,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::attachments::AttachmentReferenceKind;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn test_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project")
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
            "ResultsV1(title: \"Title\", requirement_path: \"requirements/definition\", requirement_commit: \"abc\", test_path: \"tests/generic_test\", test_commit: \"abc\")",
        )
        .unwrap();
        dir
    }

    #[test]
    fn loads_the_design_result_from_the_test_project() -> Result<(), Error> {
        let dir = test_project_dir().join("results/design");
        let result = load_result(&StdFilesystem, &FixedGit, &dir)?;

        assert_eq!(result.definition.title, "Design");
        assert_eq!(
            result.definition.requirement_path.0,
            "requirements/design"
        );
        assert!(matches!(
            result.definition.status,
            crate::result::types::StatusV1::Incomplete
        ));
        assert!(result.attachments.is_empty());

        Ok(())
    }

    #[test]
    fn missing_result_ron_is_reported() {
        let dir = valid_result_dir("missing-ron");
        std::fs::remove_file(dir.join("result.ron")).unwrap();

        let err = load_result(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_attachments_dir_is_reported() {
        let dir = valid_result_dir("missing-attachments");
        std::fs::remove_dir(dir.join("attachments")).unwrap();

        let err = load_result(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Attachments(crate::attachments::ReadAttachmentsError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_lone_module_attachment_reference_is_accepted() -> Result<(), Error> {
        let dir = valid_result_dir("lone-module-attachment");
        std::fs::write(
            dir.join("result.ron"),
            r#"ResultsV1(
                title: "Title",
                requirement_path: "requirements/definition",
                requirement_commit: "abc",
                test_path: "tests/generic_test",
                test_commit: "abc",
                attachment: ModuleAttachmentReferenceV1(name: "logo.png", path: "logo.png"),
            )"#,
        )
        .unwrap();

        let result = load_result(&StdFilesystem, &FixedGit, &dir)?;
        assert!(matches!(
            result.definition.attachment,
            Some(AttachmentReferenceKind::ModuleAttachmentReferenceV1 { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn a_lone_local_attachment_reference_is_accepted() -> Result<(), Error> {
        let dir = valid_result_dir("lone-local-attachment");
        std::fs::write(
            dir.join("result.ron"),
            r#"ResultsV1(
                title: "Title",
                requirement_path: "requirements/definition",
                requirement_commit: "abc",
                test_path: "tests/generic_test",
                test_commit: "abc",
                attachment: LocalAttachmentReferenceV1(name: "logo.png", path: "logo.png"),
            )"#,
        )
        .unwrap();

        let result = load_result(&StdFilesystem, &FixedGit, &dir)?;
        assert!(matches!(
            result.definition.attachment,
            Some(AttachmentReferenceKind::LocalAttachmentReferenceV1 { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn both_attachment_and_attachments_is_rejected() {
        let dir = valid_result_dir("ambiguous-attachment");
        std::fs::write(
            dir.join("result.ron"),
            r#"ResultsV1(
                title: "Title",
                requirement_path: "requirements/definition",
                requirement_commit: "abc",
                test_path: "tests/generic_test",
                test_commit: "abc",
                attachment: ModuleAttachmentReferenceV1(name: "logo.png", path: "logo.png"),
                attachments: [ModuleAttachmentReferenceV1(name: "logo.png", path: "logo.png")],
            )"#,
        )
        .unwrap();

        let err = load_result(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Invalid {
                source: ValidateResultsError::AmbiguousField {
                    singular: "attachment",
                    plural: "attachments",
                },
                ..
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
