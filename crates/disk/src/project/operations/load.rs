use std::path::Path;

use syscalls::{Filesystem, Git};
use thiserror::Error;

use crate::module::operations::{LoadModuleTreeError, load_module_tree};
use crate::project::types::{ProjectDefinition, ProjectOnDisk};
use crate::util::{LoadRonError, load_ron};

#[derive(Debug, Error)]
enum ErrorKind {
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
    let ProjectDefinition::RootV1(definition) = load_ron(fs, &dir.join("project.ron"))?;
    let tree = load_module_tree(fs, git, dir)?;

    Ok(ProjectOnDisk { definition, tree })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn loads_the_whole_sample_project() -> Result<(), Error> {
        let project = load_project(&StdFilesystem, &FixedGit, &sample_project_dir())?;

        assert_eq!(project.definition.name, "Capstone");
        assert_eq!(project.tree.requirements.len(), 5);
        assert_eq!(project.tree.tests.len(), 5);
        assert_eq!(project.tree.results.len(), 5);
        assert_eq!(project.tree.modules.len(), 5);

        let names: Vec<&str> = project
            .tree
            .modules
            .iter()
            .map(|submodule| submodule.name.as_str())
            .collect();
        assert!(names.contains(&"setup"));
        assert!(names.contains(&"embeddings"));

        Ok(())
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
}
