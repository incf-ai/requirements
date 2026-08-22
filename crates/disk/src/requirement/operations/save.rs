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
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn round_trips_a_requirement_stage_through_a_tempdir() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = sample_project_dir().join("requirements/definition");
        let original = load_requirement_stage(&StdFilesystem, &dir)?;

        let tempdir = std::env::temp_dir().join(format!(
            "disk-requirement-round-trip-{}-{}",
            std::process::id(),
            line!()
        ));
        save_requirement_stage(&StdFilesystem, &tempdir, &original)?;
        let reloaded = load_requirement_stage(&StdFilesystem, &tempdir)?;

        assert_eq!(original.definition.title, reloaded.definition.title);
        assert_eq!(original.requirement_text, reloaded.requirement_text);
        assert_eq!(original.requirement_guidance, reloaded.requirement_guidance);
        assert_eq!(original.test_guidance, reloaded.test_guidance);
        assert_eq!(original.attachments, reloaded.attachments);

        std::fs::remove_dir_all(&tempdir).ok();
        Ok(())
    }
}
