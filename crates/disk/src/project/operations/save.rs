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
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn round_trips_the_whole_sample_project_through_a_tempdir() -> Result<(), Box<dyn std::error::Error>>
    {
        let original = load_project(&StdFilesystem, &sample_project_dir())?;

        let tempdir = std::env::temp_dir().join(format!(
            "disk-project-round-trip-{}-{}",
            std::process::id(),
            line!()
        ));
        save_project(&StdFilesystem, &tempdir, &original)?;
        let reloaded = load_project(&StdFilesystem, &tempdir)?;

        assert_eq!(original.definition.name, reloaded.definition.name);
        assert_eq!(reloaded.tree.requirements.len(), original.tree.requirements.len());
        assert_eq!(reloaded.tree.tests.len(), original.tree.tests.len());
        assert_eq!(reloaded.tree.results.len(), original.tree.results.len());
        assert_eq!(reloaded.tree.modules.len(), original.tree.modules.len());

        std::fs::remove_dir_all(&tempdir).ok();
        Ok(())
    }
}
