use std::path::{Path, PathBuf};

use syscalls::{Filesystem, Git, InitRepositoryError};
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
    #[error("failed to init a git repository at {path}: {source}")]
    InitRepository {
        path: PathBuf,
        #[source]
        source: InitRepositoryError,
    },
    #[error("failed to save project.ron: {0}")]
    Definition(#[from] SaveRonError),
    #[error("failed to save module tree: {0}")]
    Tree(#[from] SaveModuleTreeError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn save_project(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &Path,
    project: &ProjectOnDisk,
) -> Result<(), Error> {
    save_project_inner(fs, git, dir, project).map_err(Error)
}

fn save_project_inner(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &Path,
    project: &ProjectOnDisk,
) -> Result<(), ErrorKind> {
    fs.create_dir_all(dir)
        .map_err(|source| ErrorKind::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;

    // A brand new project's directory won't be a git repository yet — the
    // on-disk format needs one (every requirement/test/result's commit is
    // looked up via git), so a first save quietly sets one up instead of
    // leaving the user to discover and run `git init` themselves.
    if !git.is_repository(dir) {
        git.init_repository(dir)
            .map_err(|source| ErrorKind::InitRepository {
                path: dir.to_path_buf(),
                source,
            })?;
    }

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

    fn test_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project")
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

    /// Recurses into `tree.modules`, since each submodule has its own
    /// `attachments`/`templates` plus its own requirements/tests/results,
    /// any of which may reference real attachment/template files.
    fn copy_tree_attachment_files(
        tree: &crate::module::types::ModuleTree,
        src_dir: &std::path::Path,
        dest_dir: &std::path::Path,
    ) {
        copy_attachment_files(
            &tree.attachments,
            &src_dir.join("attachments"),
            &dest_dir.join("attachments"),
        );
        copy_attachment_files(
            &tree.templates,
            &src_dir.join("templates"),
            &dest_dir.join("templates"),
        );
        for requirement in &tree.requirements {
            copy_attachment_files(
                &requirement.attachments,
                &src_dir
                    .join("requirements")
                    .join(requirement.name.as_str())
                    .join("attachments"),
                &dest_dir
                    .join("requirements")
                    .join(requirement.name.as_str())
                    .join("attachments"),
            );
        }
        for test in &tree.tests {
            copy_attachment_files(
                &test.attachments,
                &src_dir
                    .join("tests")
                    .join(test.name.as_str())
                    .join("attachments"),
                &dest_dir
                    .join("tests")
                    .join(test.name.as_str())
                    .join("attachments"),
            );
            copy_attachment_files(
                &test.template,
                &src_dir.join("tests").join(test.name.as_str()).join("template"),
                &dest_dir
                    .join("tests")
                    .join(test.name.as_str())
                    .join("template"),
            );
        }
        for result in &tree.results {
            copy_attachment_files(
                &result.attachments,
                &src_dir
                    .join("results")
                    .join(result.name.as_str())
                    .join("attachments"),
                &dest_dir
                    .join("results")
                    .join(result.name.as_str())
                    .join("attachments"),
            );
        }
        for module in &tree.modules {
            copy_tree_attachment_files(
                &module.tree,
                &src_dir.join("modules").join(module.name.as_str()),
                &dest_dir.join("modules").join(module.name.as_str()),
            );
        }
    }

    #[test]
    fn round_trips_the_whole_test_project_through_a_tempdir()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = load_project(&StdFilesystem, &FixedGit, &test_project_dir())?;

        let tempdir = std::env::temp_dir().join(format!(
            "disk-project-round-trip-{}-{}",
            std::process::id(),
            line!()
        ));
        copy_tree_attachment_files(&original.tree, &test_project_dir(), &tempdir);
        save_project(&StdFilesystem, &FixedGit, &tempdir, &original)?;
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

        let err = save_project(&fs, &FixedGit, &dir, &minimal_project()).unwrap_err();
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

        let err = save_project(&fs, &FixedGit, &dir, &minimal_project()).unwrap_err();
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

        let err = save_project(&fs, &FixedGit, &dir, &minimal_project()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Tree(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_into_a_non_repository_directory_inits_one() {
        use syscalls::SystemGit;

        let dir = std::env::temp_dir().join(format!(
            "disk-project-save-auto-init-{}-{}",
            std::process::id(),
            line!()
        ));

        assert!(!SystemGit.is_repository(&dir));
        save_project(&StdFilesystem, &SystemGit, &dir, &minimal_project()).unwrap();
        assert!(SystemGit.is_repository(&dir));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reports `false` for `is_repository` and a fixed error for
    /// `init_repository`, to exercise `save_project`'s
    /// `ErrorKind::InitRepository` branch without needing a real init
    /// failure (e.g. `git` missing from `PATH`).
    struct FailingInitGit;

    impl Git for FailingInitGit {
        fn commit_for_path_excluding(
            &self,
            _path: &Path,
            _excludes: &[&Path],
        ) -> Result<String, syscalls::CommitForPathError> {
            unreachable!("save_project never looks up commits")
        }

        fn is_repository(&self, _dir: &Path) -> bool {
            false
        }

        fn init_repository(&self, _dir: &Path) -> Result<(), InitRepositoryError> {
            Err(InitRepositoryError::Spawn {
                source: std::io::Error::other("injected failure"),
            })
        }
    }

    #[test]
    fn reports_errors_initializing_a_git_repository() {
        let dir = std::env::temp_dir().join(format!(
            "disk-project-save-init-repo-error-{}-{}",
            std::process::id(),
            line!()
        ));

        let err = save_project(&StdFilesystem, &FailingInitGit, &dir, &minimal_project()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::InitRepository { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }
}
