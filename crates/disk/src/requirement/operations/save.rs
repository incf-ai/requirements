use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

use crate::attachments::{WriteAttachmentsError, write_attachments};
use crate::requirement::types::{
    RequirementDefinition, RequirementOnDisk, ValidateRequirementDefinitionError,
};
use crate::util::{SaveRonError, WriteTextError, save_ron, write_optional_text, write_text};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid requirement: {0}")]
    Invalid(#[from] ValidateRequirementDefinitionError),
    #[error("failed to save requirement.ron: {0}")]
    Definition(#[from] SaveRonError),
    #[error("failed to save requirement.typ: {0}")]
    RequirementText(#[from] WriteTextError),
    #[error("failed to save requirement_guidance.typ: {source}")]
    RequirementGuidance { source: WriteTextError },
    #[error("failed to save test_guidance.typ: {source}")]
    TestGuidance { source: WriteTextError },
    #[error("failed to save attachments: {0}")]
    Attachments(#[from] WriteAttachmentsError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn save_requirement_stage(
    fs: &dyn Filesystem,
    dir: &Path,
    requirement: &RequirementOnDisk,
) -> Result<(), Error> {
    save_requirement_stage_inner(fs, dir, requirement).map_err(Error)
}

fn save_requirement_stage_inner(
    fs: &dyn Filesystem,
    dir: &Path,
    requirement: &RequirementOnDisk,
) -> Result<(), ErrorKind> {
    requirement.definition.validate()?;

    fs.create_dir_all(dir)
        .map_err(|source| ErrorKind::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;

    save_ron(
        fs,
        &dir.join("requirement.ron"),
        &RequirementDefinition::RequirementDefinitionV1(requirement.definition.clone()),
    )?;
    write_text(
        fs,
        &dir.join("requirement.typ"),
        &requirement.requirement_text,
    )?;
    write_optional_text(
        fs,
        &dir.join("requirement_guidance.typ"),
        requirement.requirement_guidance.as_deref(),
    )
    .map_err(|source| ErrorKind::RequirementGuidance { source })?;
    write_optional_text(
        fs,
        &dir.join("test_guidance.typ"),
        requirement.test_guidance.as_deref(),
    )
    .map_err(|source| ErrorKind::TestGuidance { source })?;
    write_attachments(fs, &dir.join("attachments"), &requirement.attachments)?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::requirement::operations::load::load_requirement_stage;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn test_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project")
    }

    #[test]
    fn round_trips_a_requirement_stage_through_a_tempdir() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = test_project_dir().join("requirements/external");
        let original = load_requirement_stage(&StdFilesystem, &FixedGit, &dir)?;

        let tempdir = std::env::temp_dir().join(format!(
            "disk-requirement-round-trip-{}-{}",
            std::process::id(),
            line!()
        ));
        save_requirement_stage(&StdFilesystem, &tempdir, &original)?;
        let reloaded = load_requirement_stage(&StdFilesystem, &FixedGit, &tempdir)?;

        assert_eq!(original.definition.title, reloaded.definition.title);
        assert_eq!(original.requirement_text, reloaded.requirement_text);
        assert_eq!(original.requirement_guidance, reloaded.requirement_guidance);
        assert_eq!(original.test_guidance, reloaded.test_guidance);
        assert_eq!(original.attachments, reloaded.attachments);
        assert_eq!(original.commit, reloaded.commit);

        std::fs::remove_dir_all(&tempdir).ok();
        Ok(())
    }

    fn minimal_requirement() -> RequirementOnDisk {
        RequirementOnDisk {
            name: crate::util::EntryName("definition".to_string()),
            definition: crate::requirement::types::RequirementDefinitionV1 {
                title: "Title".to_string(),
                test: None,
                tests: None,
                dependency: None,
                dependencies: None,
                attachment: None,
                attachments: None,
                include_attachments_in_commit: true,
            },
            requirement_text: String::new(),
            requirement_guidance: None,
            test_guidance: None,
            attachments: Vec::new(),
            commit: Some("deadbeef".to_string()),
        }
    }

    #[test]
    fn reports_io_errors_saving_requirement_ron() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-requirement-save-definition-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("requirement.ron"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = save_requirement_stage(&fs, &dir, &minimal_requirement()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Definition(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_requirement_guidance() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-requirement-save-guidance-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("requirement_guidance.typ"),
            std::io::ErrorKind::PermissionDenied,
        );

        let mut requirement = minimal_requirement();
        requirement.requirement_guidance = Some("guidance".to_string());
        let err = save_requirement_stage(&fs, &dir, &requirement).unwrap_err();
        assert!(matches!(err.0, ErrorKind::RequirementGuidance { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_test_guidance() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-requirement-save-test-guidance-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("test_guidance.typ"),
            std::io::ErrorKind::PermissionDenied,
        );

        let mut requirement = minimal_requirement();
        requirement.test_guidance = Some("guidance".to_string());
        let err = save_requirement_stage(&fs, &dir, &requirement).unwrap_err();
        assert!(matches!(err.0, ErrorKind::TestGuidance { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_requirement_typ() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-requirement-save-text-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("requirement.typ"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = save_requirement_stage(&fs, &dir, &minimal_requirement()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::RequirementText(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_attachments() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-requirement-save-attachments-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("attachments"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = save_requirement_stage(&fs, &dir, &minimal_requirement()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Attachments(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_an_ambiguous_definition_before_writing_anything() {
        use crate::requirement::types::ReferencePath;
        use crate::requirement::types::{LocalGitReference, TestReferenceKind};

        let dir = std::env::temp_dir().join(format!(
            "disk-requirement-save-invalid-{}-{}",
            std::process::id(),
            line!()
        ));

        let mut requirement = minimal_requirement();
        requirement.definition.test = Some(TestReferenceKind::TestReferenceV1(LocalGitReference {
            path: ReferencePath("/tests/generic_test".to_string()),
            commit: "deadbeef".to_string(),
        }));
        requirement.definition.tests = Some(nunny::vec![TestReferenceKind::TestReferenceV1(
            LocalGitReference {
                path: ReferencePath("/tests/generic_test".to_string()),
                commit: "deadbeef".to_string(),
            }
        )]);

        let err = save_requirement_stage(&StdFilesystem, &dir, &requirement).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Invalid(_)));
        assert!(!dir.exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
