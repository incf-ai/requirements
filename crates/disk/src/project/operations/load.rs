use std::path::Path;

use syscalls::{Filesystem, Git};
use thiserror::Error;

use crate::module::operations::{LoadModuleTreeError, load_module_tree};
use crate::project::types::{ProjectDefinition, ProjectOnDisk};
use crate::util::{LoadRonError, load_ron};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("{} is not a git repository", path.display())]
    NotAGitRepository { path: std::path::PathBuf },
    #[error("failed to load project.ron: {0}")]
    Definition(#[from] LoadRonError),
    #[error("failed to load module tree: {0}")]
    Tree(#[from] LoadModuleTreeError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn load_project(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &Path,
) -> Result<ProjectOnDisk, Error> {
    load_project_inner(fs, git, dir).map_err(Error)
}

fn load_project_inner(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &Path,
) -> Result<ProjectOnDisk, ErrorKind> {
    // Checked first, and separately from `load_module_tree`'s own git
    // lookups: every requirement/test/result under `dir` needs a commit
    // looked up for it, so without this a missing repo only surfaces deep
    // inside the tree, as a confusing per-leaf git failure instead of one
    // clear message about the actual problem.
    if !git.is_repository(dir) {
        return Err(ErrorKind::NotAGitRepository {
            path: dir.to_path_buf(),
        });
    }

    let ProjectDefinition::RootV1(definition) = load_ron(fs, &dir.join("project.ron"))?;
    let tree = load_module_tree(fs, git, dir)?;

    Ok(ProjectOnDisk { definition, tree })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn test_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project")
    }

    #[test]
    fn loads_the_whole_test_project() -> Result<(), Error> {
        let project = load_project(&StdFilesystem, &FixedGit, &test_project_dir())?;

        assert_eq!(project.definition.name, "Test Project");
        assert_eq!(project.tree.requirements.len(), 3);
        assert_eq!(project.tree.tests.len(), 3);
        assert_eq!(project.tree.results.len(), 3);
        assert_eq!(project.tree.modules.len(), 2);

        let names: Vec<&str> = project
            .tree
            .modules
            .iter()
            .map(|submodule| submodule.name.as_str())
            .collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));

        Ok(())
    }

    /// Reports `false` for every path, no matter what's actually there —
    /// used to exercise the "not a git repository" check without needing a
    /// real non-repo directory (which `FixedGit`'s default `is_repository`
    /// wouldn't give us, since it just inherits the trait's `true` default).
    struct NotARepoGit;

    impl syscalls::Git for NotARepoGit {
        fn commit_for_path_excluding(
            &self,
            _path: &std::path::Path,
            _excludes: &[&std::path::Path],
        ) -> Result<String, syscalls::CommitForPathError> {
            unreachable!("load_project should bail out before looking up any commits")
        }

        fn is_repository(&self, _dir: &std::path::Path) -> bool {
            false
        }

        fn changed_paths(&self, _dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, syscalls::ChangedPathsError> {
            unreachable!("load_project should bail out before looking up any commits")
        }

        fn commit_all(&self, _dir: &std::path::Path, _message: &str) -> Result<(), syscalls::CommitAllError> {
            unreachable!("load_project should bail out before looking up any commits")
        }
    }

    #[test]
    fn a_missing_git_repository_is_reported() {
        let dir = std::env::temp_dir().join(format!(
            "disk-project-load-not-a-repo-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let err = load_project(&StdFilesystem, &NotARepoGit, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::NotAGitRepository { path } if path == dir));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_project_ron_is_reported() {
        let dir = std::env::temp_dir().join(format!(
            "disk-project-load-missing-ron-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join("requirements")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::create_dir_all(dir.join("results")).unwrap();
        std::fs::create_dir_all(dir.join("modules")).unwrap();

        let err = load_project(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failing_module_tree_load_is_reported() {
        let dir = std::env::temp_dir().join(format!(
            "disk-project-load-tree-error-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project.ron"), "RootV1(name: \"Demo\")").unwrap();
        // Deliberately no `attachments/`/`templates/`/`requirements/`/etc.
        // subdirectories — `load_module_tree` fails on the first one it
        // looks for.

        let err = load_project(&StdFilesystem, &FixedGit, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Tree(_)));

        std::fs::remove_dir_all(&dir).ok();
    }
}
