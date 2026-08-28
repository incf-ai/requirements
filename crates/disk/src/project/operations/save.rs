use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

use crate::module::operations::{SaveModuleTreeError, save_module_tree};
use crate::project::types::{ProjectDefinition, ProjectOnDisk};
use crate::util::{SaveRonError, save_ron};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to save project.ron: {0}")]
    Definition(#[from] SaveRonError),
    #[error("failed to save module tree: {0}")]
    Tree(#[from] SaveModuleTreeError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn save_project(fs: &dyn Filesystem, dir: &Path, project: &ProjectOnDisk) -> Result<(), Error> {
    save_project_inner(fs, dir, project).map_err(Error)
}

fn save_project_inner(
    fs: &dyn Filesystem,
    dir: &Path,
    project: &ProjectOnDisk,
) -> Result<(), ErrorKind> {
    fs.create_dir_all(dir)
        .map_err(|source| ErrorKind::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;

    save_ron(
        fs,
        &dir.join("project.ron"),
        &ProjectDefinition::RootV1(project.definition.clone()),
    )?;
    save_module_tree(fs, dir, &project.tree)?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::project::operations::load::load_project;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    /// `write_attachments` only validates that referenced attachment files
    /// already exist on disk (it never writes bytes), so every real
    /// attachment/template file has to be copied into place before saving —
    /// `src_dir`/`dest_dir` are the `attachments/` or `template/` directory
    /// itself, matching `AttachmentFile.path`'s "relative to that directory"
    /// convention.
    fn copy_attachment_files(
        attachments: &[crate::attachments::AttachmentFile],
        src_dir: &std::path::Path,
        dest_dir: &std::path::Path,
    ) {
        for attachment in attachments {
            let src = src_dir.join(&attachment.path);
            let dest = dest_dir.join(&attachment.path);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(src, dest).unwrap();
        }
    }

    #[test]
    fn round_trips_the_whole_sample_project_through_a_tempdir()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = load_project(&StdFilesystem, &FixedGit, &sample_project_dir())?;

        let tempdir = std::env::temp_dir().join(format!(
            "disk-project-round-trip-{}-{}",
            std::process::id(),
            line!()
        ));
        copy_attachment_files(
            &original.tree.attachments,
            &sample_project_dir().join("attachments"),
            &tempdir.join("attachments"),
        );
        copy_attachment_files(
            &original.tree.templates,
            &sample_project_dir().join("templates"),
            &tempdir.join("templates"),
        );
        for requirement in &original.tree.requirements {
            copy_attachment_files(
                &requirement.attachments,
                &sample_project_dir()
                    .join("requirements")
                    .join(requirement.name.as_str())
                    .join("attachments"),
                &tempdir
                    .join("requirements")
                    .join(requirement.name.as_str())
                    .join("attachments"),
            );
        }
        for test in &original.tree.tests {
            copy_attachment_files(
                &test.attachments,
                &sample_project_dir()
                    .join("tests")
                    .join(test.name.as_str())
                    .join("attachments"),
                &tempdir
                    .join("tests")
                    .join(test.name.as_str())
                    .join("attachments"),
            );
            copy_attachment_files(
                &test.template,
                &sample_project_dir()
                    .join("tests")
                    .join(test.name.as_str())
                    .join("template"),
                &tempdir
                    .join("tests")
                    .join(test.name.as_str())
                    .join("template"),
            );
        }
        for result in &original.tree.results {
            copy_attachment_files(
                &result.attachments,
                &sample_project_dir()
                    .join("results")
                    .join(result.name.as_str())
                    .join("attachments"),
                &tempdir
                    .join("results")
                    .join(result.name.as_str())
                    .join("attachments"),
            );
        }
        save_project(&StdFilesystem, &tempdir, &original)?;
        let reloaded = load_project(&StdFilesystem, &FixedGit, &tempdir)?;

        assert_eq!(original.definition.name, reloaded.definition.name);
        assert_eq!(
            reloaded.tree.requirements.len(),
            original.tree.requirements.len()
        );
        assert_eq!(reloaded.tree.tests.len(), original.tree.tests.len());
        assert_eq!(reloaded.tree.results.len(), original.tree.results.len());
        assert_eq!(reloaded.tree.modules.len(), original.tree.modules.len());

        std::fs::remove_dir_all(&tempdir).ok();
        Ok(())
    }

    fn minimal_project() -> ProjectOnDisk {
        ProjectOnDisk {
            definition: crate::project::types::RootV1 {
                name: "Capstone".to_string(),
            },
            tree: crate::module::types::ModuleTree::default(),
        }
    }

    #[test]
    fn reports_io_errors_creating_the_directory() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-project-save-create-dir-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&dir, std::io::ErrorKind::PermissionDenied);

        let err = save_project(&fs, &dir, &minimal_project()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::CreateDir { .. }));
    }

    #[test]
    fn reports_io_errors_saving_project_ron() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-project-save-definition-io-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("project.ron"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = save_project(&fs, &dir, &minimal_project()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Definition(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failing_module_tree_save_is_reported() {
        use syscalls::FaultInjectingFilesystem;

        let dir = std::env::temp_dir().join(format!(
            "disk-project-save-tree-error-{}-{}",
            std::process::id(),
            line!()
        ));
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("attachments"),
            std::io::ErrorKind::PermissionDenied,
        );

        let err = save_project(&fs, &dir, &minimal_project()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Tree(_)));

        std::fs::remove_dir_all(&dir).ok();
    }
}
