use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

use crate::attachments::{WriteAttachmentsError, write_attachments};
use crate::test::types::{TestDefinition, TestOnDisk};
use crate::util::{SaveRonError, WriteTextError, save_ron, write_text};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to save test.ron: {0}")]
    Definition(#[from] SaveRonError),
    #[error("failed to save test.typ: {0}")]
    TestText(#[from] WriteTextError),
    #[error("failed to save attachments: {0}")]
    Attachments(#[from] WriteAttachmentsError),
    #[error("failed to save template: {source}")]
    Template { source: WriteAttachmentsError },
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn save_test(fs: &dyn Filesystem, dir: &Path, test: &TestOnDisk) -> Result<(), Error> {
    save_test_inner(fs, dir, test).map_err(Error)
}

fn save_test_inner(fs: &dyn Filesystem, dir: &Path, test: &TestOnDisk) -> Result<(), ErrorKind> {
    fs.create_dir_all(dir)
        .map_err(|source| ErrorKind::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;

    save_ron(
        fs,
        &dir.join("test.ron"),
        &TestDefinition::TestV1(test.definition.clone()),
    )?;
    write_text(fs, &dir.join("test.typ"), &test.test_text)?;
    write_attachments(fs, &dir.join("attachments"), &test.attachments)?;
    write_attachments(fs, &dir.join("template"), &test.template)
        .map_err(|source| ErrorKind::Template { source })?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::operations::load::load_test;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn round_trips_a_test_through_a_tempdir() -> Result<(), Box<dyn std::error::Error>> {
        let dir = sample_project_dir().join("tests/generic_test");
        let original = load_test(&StdFilesystem, &FixedGit, &dir)?;

        let tempdir = std::env::temp_dir().join(format!(
            "disk-test-round-trip-{}-{}",
            std::process::id(),
            line!()
        ));
        // `write_attachments` only validates that referenced attachment
        // files already exist on disk (it never writes bytes), so the
        // template file has to be copied into place before saving.
        std::fs::create_dir_all(tempdir.join("template")).unwrap();
        std::fs::copy(
            dir.join("template/result.typ"),
            tempdir.join("template/result.typ"),
        )
        .unwrap();

        save_test(&StdFilesystem, &tempdir, &original)?;
        let reloaded = load_test(&StdFilesystem, &FixedGit, &tempdir)?;

        assert_eq!(original.definition.title, reloaded.definition.title);
        assert_eq!(original.test_text, reloaded.test_text);
        assert_eq!(original.attachments, reloaded.attachments);
        assert_eq!(original.template, reloaded.template);

        std::fs::remove_dir_all(&tempdir).ok();
        Ok(())
    }

    fn minimal_test() -> TestOnDisk {
        TestOnDisk {
            name: crate::util::EntryName("generic_test".to_string()),
            definition: crate::test::types::TestV1 {
                title: "Title".to_string(),
                result_kind: crate::test::types::ResultKindV1::FreeForm,
                attachment: None,
                attachments: None,
                template: None,
                templates: None,
                include_attachments_in_commit: true,
                include_template_in_commit: true,
            },
            test_text: String::new(),
            attachments: Vec::new(),
            template: Vec::new(),
            commit: "deadbeef".to_string(),
        }
    }

    #[test]
    fn leaves_a_gitkeep_in_attachments_and_template() {
        let dir = std::env::temp_dir().join(format!(
            "disk-test-save-gitkeep-{}-{}",
            std::process::id(),
            line!()
        ));

        save_test(&StdFilesystem, &dir, &minimal_test()).unwrap();

        assert!(dir.join("attachments/.gitkeep").exists());
        assert!(dir.join("template/.gitkeep").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_the_template() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-test-save-template-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("template"), std::io::ErrorKind::PermissionDenied);

        let mut test = minimal_test();
        test.template = vec![crate::attachments::AttachmentFile {
            path: std::path::PathBuf::from("result.typ"),
            commit: "deadbeef".to_string(),
        }];
        let err = save_test(&fs, &dir, &test).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Template { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_test_ron() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-test-save-definition-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("test.ron"), std::io::ErrorKind::PermissionDenied);

        let err = save_test(&fs, &dir, &minimal_test()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Definition(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_test_typ() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-test-save-text-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("test.typ"), std::io::ErrorKind::PermissionDenied);

        let err = save_test(&fs, &dir, &minimal_test()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::TestText(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_io_errors_saving_attachments() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-test-save-attachments-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("attachments"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = save_test(&fs, &dir, &minimal_test()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Attachments(_)));

        std::fs::remove_dir_all(&dir).ok();
    }
}
