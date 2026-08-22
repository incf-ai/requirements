use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

use crate::attachments::{ReadAttachmentsError, read_attachments};
use crate::requirement::types::{
    RequirementDefinition, RequirementOnDisk, ValidateRequirementDefinitionError,
};
use crate::util::{
    EntryName, LoadRonError, ReadOptionalTextError, ReadRequiredTextError, load_ron,
    read_optional_text, read_required_text,
};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to load requirement.ron: {0}")]
    Definition(#[from] LoadRonError),
    #[error("invalid requirement.ron at {path}: {source}")]
    Invalid {
        path: PathBuf,
        #[source]
        source: ValidateRequirementDefinitionError,
    },
    #[error("failed to load requirement.typ: {0}")]
    RequirementText(#[from] ReadRequiredTextError),
    #[error("failed to load requirement_guidance.typ: {source}")]
    RequirementGuidance { source: ReadOptionalTextError },
    #[error("failed to load test_guidance.typ: {source}")]
    TestGuidance { source: ReadOptionalTextError },
    #[error("failed to load attachments: {0}")]
    Attachments(#[from] ReadAttachmentsError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn load_requirement_stage(
    fs: &dyn Filesystem,
    dir: &Path,
) -> Result<RequirementOnDisk, Error> {
    load_requirement_stage_inner(fs, dir).map_err(Error)
}

fn load_requirement_stage_inner(
    fs: &dyn Filesystem,
    dir: &Path,
) -> Result<RequirementOnDisk, ErrorKind> {
    let ron_path = dir.join("requirement.ron");
    let RequirementDefinition::RequirementDefinitionV1(definition) = load_ron(fs, &ron_path)?;
    definition
        .validate()
        .map_err(|source| ErrorKind::Invalid {
            path: ron_path.clone(),
            source,
        })?;

    let requirement_text = read_required_text(fs, &dir.join("requirement.typ"))?;
    let requirement_guidance = read_optional_text(fs, &dir.join("requirement_guidance.typ"))
        .map_err(|source| ErrorKind::RequirementGuidance { source })?;
    let test_guidance = read_optional_text(fs, &dir.join("test_guidance.typ"))
        .map_err(|source| ErrorKind::TestGuidance { source })?;
    let attachments = read_attachments(fs, &dir.join("attachments"))?;

    Ok(RequirementOnDisk {
        name: EntryName::of(dir),
        definition,
        requirement_text,
        requirement_guidance,
        test_guidance,
        attachments,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::requirement::types::DependencyReferenceKind;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    /// Builds a minimal valid `requirements/<stage>/` folder in a fresh tempdir.
    fn valid_stage_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "disk-requirement-load-{name}-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        std::fs::write(
            dir.join("requirement.ron"),
            "RequirementDefinitionV1(title: \"Title\")",
        )
        .unwrap();
        std::fs::write(dir.join("requirement.typ"), "").unwrap();
        dir
    }

    #[test]
    fn loads_the_definition_stage_from_the_sample_project() -> Result<(), Error> {
        let dir = sample_project_dir().join("requirements/definition");
        let requirement = load_requirement_stage(&StdFilesystem, &dir)?;

        assert_eq!(requirement.definition.title, "Definition");
        assert_eq!(requirement.requirement_text, "");
        assert_eq!(requirement.requirement_guidance, Some(String::new()));
        assert_eq!(requirement.test_guidance, Some(String::new()));
        assert!(requirement.attachments.is_empty());

        Ok(())
    }

    #[test]
    fn loads_the_implementation_stage_submodules_dependency() -> Result<(), Error> {
        let dir = sample_project_dir().join("requirements/implementation");
        let requirement = load_requirement_stage(&StdFilesystem, &dir)?;

        assert!(matches!(
            requirement.definition.dependency,
            Some(DependencyReferenceKind::Submodules)
        ));

        Ok(())
    }

    #[test]
    fn missing_requirement_ron_is_reported() {
        let dir = valid_stage_dir("missing-ron");
        std::fs::remove_file(dir.join("requirement.ron")).unwrap();

        let err = load_requirement_stage(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn io_errors_reading_requirement_guidance_are_reported() {
        use syscalls::FaultInjectingFilesystem;

        let dir = valid_stage_dir("guidance-io");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("requirement_guidance.typ"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = load_requirement_stage(&fs, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::RequirementGuidance { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn io_errors_reading_test_guidance_are_reported() {
        use syscalls::FaultInjectingFilesystem;

        let dir = valid_stage_dir("test-guidance-io");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("test_guidance.typ"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = load_requirement_stage(&fs, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::TestGuidance { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_requirement_typ_is_reported() {
        let dir = valid_stage_dir("missing-typ");
        std::fs::remove_file(dir.join("requirement.typ")).unwrap();

        let err = load_requirement_stage(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::RequirementText(ReadRequiredTextError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_attachments_dir_is_reported() {
        let dir = valid_stage_dir("missing-attachments");
        std::fs::remove_dir(dir.join("attachments")).unwrap();

        let err = load_requirement_stage(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Attachments(ReadAttachmentsError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_requirement_ron_is_reported() {
        let dir = valid_stage_dir("malformed-ron");
        std::fs::write(dir.join("requirement.ron"), "not valid ron {{{").unwrap();

        let err = load_requirement_stage(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Parse { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn both_test_and_tests_is_rejected() {
        let dir = valid_stage_dir("ambiguous-test");
        std::fs::write(
            dir.join("requirement.ron"),
            r#"RequirementDefinitionV1(
                title: "Title",
                test: TestReferenceV1(path: "/tests/generic_test", commit: "abc"),
                tests: [TestReferenceV1(path: "/tests/generic_test", commit: "abc")],
            )"#,
        )
        .unwrap();

        let err = load_requirement_stage(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Invalid {
                source: ValidateRequirementDefinitionError::AmbiguousField {
                    singular: "test",
                    plural: "tests",
                },
                ..
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn both_dependency_and_dependencies_is_rejected() {
        let dir = valid_stage_dir("ambiguous-dependency");
        std::fs::write(
            dir.join("requirement.ron"),
            r#"RequirementDefinitionV1(
                title: "Title",
                dependency: Submodules,
                dependencies: [Submodules],
            )"#,
        )
        .unwrap();

        let err = load_requirement_stage(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Invalid {
                source: ValidateRequirementDefinitionError::AmbiguousField {
                    singular: "dependency",
                    plural: "dependencies",
                },
                ..
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
