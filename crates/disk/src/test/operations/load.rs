use std::path::Path;

use syscalls::Filesystem;
use thiserror::Error;

use crate::attachments::{ReadAttachmentsError, read_attachments};
use crate::test::types::{TestDefinition, TestOnDisk};
use crate::util::{EntryName, LoadRonError, ReadRequiredTextError, load_ron, read_required_text};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to load test.ron: {0}")]
    Definition(#[from] LoadRonError),
    #[error("failed to load test.typ: {0}")]
    TestText(#[from] ReadRequiredTextError),
    #[error("failed to load attachments: {0}")]
    Attachments(#[from] ReadAttachmentsError),
    #[error("failed to load template: {source}")]
    Template { source: ReadAttachmentsError },
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn load_test(fs: &dyn Filesystem, dir: &Path) -> Result<TestOnDisk, Error> {
    load_test_inner(fs, dir).map_err(Error)
}

fn load_test_inner(fs: &dyn Filesystem, dir: &Path) -> Result<TestOnDisk, ErrorKind> {
    let TestDefinition::TestV1(definition) = load_ron(fs, &dir.join("test.ron"))?;

    let test_text = read_required_text(fs, &dir.join("test.typ"))?;
    let attachments = read_attachments(fs, &dir.join("attachments"))?;
    let template = read_attachments(fs, &dir.join("template"))
        .map_err(|source| ErrorKind::Template { source })?;

    Ok(TestOnDisk {
        name: EntryName::of(dir),
        definition,
        test_text,
        attachments,
        template,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::types::ResultKindV1;
    use syscalls::StdFilesystem;

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
    fn loads_generic_test_from_the_sample_project() -> Result<(), Error> {
        let dir = sample_project_dir().join("tests/generic_test");
        let test = load_test(&StdFilesystem, &dir)?;

        assert_eq!(test.definition.title, "Generic Test");
        assert!(matches!(test.definition.result_kind, ResultKindV1::FreeForm));
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

        let err = load_test(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_test_typ_is_reported() {
        let dir = valid_test_dir("missing-typ");
        std::fs::remove_file(dir.join("test.typ")).unwrap();

        let err = load_test(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::TestText(ReadRequiredTextError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_template_dir_is_reported() {
        let dir = valid_test_dir("missing-template");
        std::fs::remove_dir(dir.join("template")).unwrap();

        let err = load_test(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Template {
                source: ReadAttachmentsError::Missing { .. },
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
