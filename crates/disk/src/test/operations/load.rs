use std::path::{Path, PathBuf};

use syscalls::{CommitForPathError, Filesystem, Git};
use thiserror::Error;

use crate::attachments::{ReadAttachmentsError, read_attachments};
use crate::test::types::{TestDefinition, TestOnDisk, ValidateTestError};
use crate::util::{EntryName, LoadRonError, ReadRequiredTextError, load_ron, read_required_text};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to load test.ron: {0}")]
    Definition(#[from] LoadRonError),
    #[error("invalid test.ron at {path}: {source}")]
    Invalid {
        path: PathBuf,
        #[source]
        source: ValidateTestError,
    },
    #[error("failed to load test.typ: {0}")]
    TestText(#[from] ReadRequiredTextError),
    #[error("failed to load attachments: {0}")]
    Attachments(#[from] ReadAttachmentsError),
    #[error("failed to load template: {source}")]
    Template { source: ReadAttachmentsError },
    #[error("failed to look up newest commit for {path}: {source}")]
    Commit {
        path: PathBuf,
        #[source]
        source: CommitForPathError,
    },
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn load_test(fs: &dyn Filesystem, git: &dyn Git, dir: &Path) -> Result<TestOnDisk, Error> {
    load_test_inner(fs, git, dir).map_err(Error)
}

fn load_test_inner(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &Path,
) -> Result<TestOnDisk, ErrorKind> {
    let ron_path = dir.join("test.ron");
    let TestDefinition::TestV1(definition) = load_ron(fs, &ron_path)?;
    definition.validate().map_err(|source| ErrorKind::Invalid {
        path: ron_path.clone(),
        source,
    })?;

    let test_text = read_required_text(fs, &dir.join("test.typ"))?;
    let attachments = read_attachments(fs, git, &dir.join("attachments"))?;
    let template = read_attachments(fs, git, &dir.join("template"))
        .map_err(|source| ErrorKind::Template { source })?;

    let attachments_dir = dir.join("attachments");
    let template_dir = dir.join("template");
    let mut excludes: Vec<&Path> = Vec::new();
    if !definition.include_attachments_in_commit {
        excludes.push(attachments_dir.as_path());
    }
    if !definition.include_template_in_commit {
        excludes.push(template_dir.as_path());
    }
    let commit = git
        .commit_for_path_excluding(dir, &excludes)
        .map_err(|source| ErrorKind::Commit {
            path: dir.to_path_buf(),
            source,
        })?;

    Ok(TestOnDisk {
        name: EntryName::of(dir),
        definition,
        test_text,
        attachments,
        template,
        commit,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::attachments::AttachmentReferenceKind;
    use crate::test::types::{ResultKindV1, TemplateReferenceKind};
    use crate::test_support::{FixedGit, git_commit_all, init_scratch_git_repo};
    use syscalls::{StdFilesystem, SystemGit};

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    /// Builds a minimal valid `tests/<name>/` folder in a fresh tempdir.
    fn valid_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "disk-test-load-{name}-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        std::fs::create_dir_all(dir.join("template")).unwrap();
        std::fs::write(
            dir.join("test.ron"),
            "TestV1(title: \"Title\", result_kind: FreeForm)",
        )
        .unwrap();
        std::fs::write(dir.join("test.typ"), "").unwrap();
        dir
    }

    #[test]
    fn gitkeep_placeholders_are_excluded_from_attachments_and_template() -> Result<(), Error> {
        let dir = valid_test_dir("gitkeep-excluded");
        std::fs::write(dir.join("attachments/.gitkeep"), "").unwrap();
        std::fs::write(dir.join("attachments/real.txt"), "hello").unwrap();
        std::fs::write(dir.join("template/.gitkeep"), "").unwrap();
        std::fs::write(dir.join("template/result.typ"), "hello").unwrap();

        let test = load_test(&StdFilesystem, &FixedGit, &dir)?;
        assert_eq!(test.attachments.len(), 1);
        assert_eq!(test.attachments[0].path, std::path::Path::new("real.txt"));
        assert_eq!(test.template.len(), 1);
        assert_eq!(test.template[0].path, std::path::Path::new("result.typ"));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn loads_generic_test_from_the_sample_project() -> Result<(), Error> {
        let dir = sample_project_dir().join("tests/generic_test");
        let test = load_test(&StdFilesystem, &FixedGit, &dir)?;

        assert_eq!(test.definition.title, "Generic Test");
        assert!(matches!(
            test.definition.result_kind,
            ResultKindV1::FreeForm
        ));
        assert_eq!(
            test.test_text,
            "Perform an test that the requirement(s) are met recording applicable data.\n"
        );
        assert!(test.attachments.is_empty());
        assert_eq!(test.template.len(), 1);
        assert_eq!(test.template[0].path, std::path::Path::new("result.typ"));

        Ok(())
    }

    #[test]
    fn missing_test_ron_is_reported() {
        let dir = valid_test_dir("missing-ron");
        std::fs::remove_file(dir.join("test.ron")).unwrap();

        let err = load_test(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_lookup_errors_are_reported() {
        use syscalls::FaultInjectingGit;

        let dir = valid_test_dir("commit-io");
        let mut git = FaultInjectingGit::new(FixedGit);
        git.inject(&dir, std::io::ErrorKind::PermissionDenied);

        let err = load_test(&StdFilesystem, &git, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Commit { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_test_typ_is_reported() {
        let dir = valid_test_dir("missing-typ");
        std::fs::remove_file(dir.join("test.typ")).unwrap();

        let err = load_test(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::TestText(ReadRequiredTextError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_lone_module_attachment_reference_is_accepted() -> Result<(), Error> {
        let dir = valid_test_dir("lone-module-attachment");
        std::fs::write(
            dir.join("test.ron"),
            r#"TestV1(
                title: "Title",
                result_kind: FreeForm,
                attachment: ModuleAttachmentReferenceV1(name: "logo.png", path: "logo.png"),
            )"#,
        )
        .unwrap();

        let test = load_test(&StdFilesystem, &FixedGit, &dir)?;
        assert!(matches!(
            test.definition.attachment,
            Some(AttachmentReferenceKind::ModuleAttachmentReferenceV1 { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn a_lone_local_attachment_reference_is_accepted() -> Result<(), Error> {
        let dir = valid_test_dir("lone-local-attachment");
        std::fs::write(
            dir.join("test.ron"),
            r#"TestV1(
                title: "Title",
                result_kind: FreeForm,
                attachment: LocalAttachmentReferenceV1(name: "logo.png", path: "logo.png"),
            )"#,
        )
        .unwrap();

        let test = load_test(&StdFilesystem, &FixedGit, &dir)?;
        assert!(matches!(
            test.definition.attachment,
            Some(AttachmentReferenceKind::LocalAttachmentReferenceV1 { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn both_attachment_and_attachments_is_rejected() {
        let dir = valid_test_dir("ambiguous-attachment");
        std::fs::write(
            dir.join("test.ron"),
            r#"TestV1(
                title: "Title",
                result_kind: FreeForm,
                attachment: ModuleAttachmentReferenceV1(name: "logo.png", path: "logo.png"),
                attachments: [ModuleAttachmentReferenceV1(name: "logo.png", path: "logo.png")],
            )"#,
        )
        .unwrap();

        let err = load_test(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Invalid {
                source: ValidateTestError::AmbiguousField {
                    singular: "attachment",
                    plural: "attachments",
                },
                ..
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_lone_local_template_reference_is_accepted() -> Result<(), Error> {
        let dir = valid_test_dir("lone-local-template");
        std::fs::write(
            dir.join("test.ron"),
            r#"TestV1(
                title: "Title",
                result_kind: FreeForm,
                template: LocalTemplateReferenceV1(name: "result", path: "result.typ"),
            )"#,
        )
        .unwrap();

        let test = load_test(&StdFilesystem, &FixedGit, &dir)?;
        assert!(matches!(
            test.definition.template,
            Some(TemplateReferenceKind::LocalTemplateReferenceV1 { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn a_lone_module_template_reference_is_accepted() -> Result<(), Error> {
        let dir = valid_test_dir("lone-module-template");
        std::fs::write(
            dir.join("test.ron"),
            r#"TestV1(
                title: "Title",
                result_kind: FreeForm,
                template: ModuleTemplateReferenceV1(name: "result", path: "result.typ"),
            )"#,
        )
        .unwrap();

        let test = load_test(&StdFilesystem, &FixedGit, &dir)?;
        assert!(matches!(
            test.definition.template,
            Some(TemplateReferenceKind::ModuleTemplateReferenceV1 { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn both_template_and_templates_is_rejected() {
        let dir = valid_test_dir("ambiguous-template");
        std::fs::write(
            dir.join("test.ron"),
            r#"TestV1(
                title: "Title",
                result_kind: FreeForm,
                template: ModuleTemplateReferenceV1(name: "result", path: "result.typ"),
                templates: [ModuleTemplateReferenceV1(name: "result", path: "result.typ")],
            )"#,
        )
        .unwrap();

        let err = load_test(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Invalid {
                source: ValidateTestError::AmbiguousField {
                    singular: "template",
                    plural: "templates",
                },
                ..
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_template_dir_is_reported() {
        let dir = valid_test_dir("missing-template");
        std::fs::remove_dir(dir.join("template")).unwrap();

        let err = load_test(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Template {
                source: ReadAttachmentsError::Missing { .. },
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn valid_git_backed_test_dir(name: &str, ron: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "disk-test-load-git-{name}-{}-{}",
            std::process::id(),
            line!()
        ));
        init_scratch_git_repo(&dir);
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        std::fs::create_dir_all(dir.join("template")).unwrap();
        std::fs::write(dir.join("test.ron"), ron).unwrap();
        std::fs::write(dir.join("test.typ"), "").unwrap();
        dir
    }

    #[test]
    fn include_attachments_in_commit_false_excludes_attachments_from_the_commit() {
        let dir = valid_git_backed_test_dir(
            "exclude-attachments",
            r#"TestV1(title: "Title", result_kind: FreeForm, include_attachments_in_commit: false)"#,
        );
        let base_commit = git_commit_all(&dir, "initial");

        std::fs::write(dir.join("attachments/extra.txt"), "hello").unwrap();
        git_commit_all(&dir, "touch attachments only");

        let test = load_test(&StdFilesystem, &SystemGit, &dir).unwrap();
        assert_eq!(test.commit, base_commit);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_template_in_commit_false_excludes_template_from_the_commit() {
        let dir = valid_git_backed_test_dir(
            "exclude-template",
            r#"TestV1(title: "Title", result_kind: FreeForm, include_template_in_commit: false)"#,
        );
        let base_commit = git_commit_all(&dir, "initial");

        std::fs::write(dir.join("template/result.typ"), "hello").unwrap();
        git_commit_all(&dir, "touch template only");

        let test = load_test(&StdFilesystem, &SystemGit, &dir).unwrap();
        assert_eq!(test.commit, base_commit);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_includes_attachments_and_template_by_default() {
        let dir = valid_git_backed_test_dir(
            "include-both",
            r#"TestV1(title: "Title", result_kind: FreeForm)"#,
        );
        let base_commit = git_commit_all(&dir, "initial");

        std::fs::write(dir.join("template/result.typ"), "hello").unwrap();
        let latest_commit = git_commit_all(&dir, "touch template only");

        let test = load_test(&StdFilesystem, &SystemGit, &dir).unwrap();
        assert_ne!(test.commit, base_commit);
        assert_eq!(test.commit, latest_commit);

        std::fs::remove_dir_all(&dir).ok();
    }
}
