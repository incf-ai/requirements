use std::io;
use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

use crate::module::operations::{SaveModuleTreeError, save_module_tree};
use crate::module::types::{SubmoduleDefinition, SubmoduleOnDisk};
use crate::util::{SaveRonError, save_ron};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to save submodule.ron: {0}")]
    Definition(#[from] SaveRonError),
    #[error("failed to save module tree: {0}")]
    Tree(#[from] Box<SaveModuleTreeError>),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn save_submodule(
    fs: &dyn Filesystem,
    dir: &Path,
    submodule: &SubmoduleOnDisk,
) -> Result<(), Error> {
    save_submodule_inner(fs, dir, submodule).map_err(Error)
}

fn save_submodule_inner(
    fs: &dyn Filesystem,
    dir: &Path,
    submodule: &SubmoduleOnDisk,
) -> Result<(), ErrorKind> {
    fs.create_dir_all(dir)
        .map_err(|source| ErrorKind::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;

    save_ron(
        fs,
        &dir.join("submodule.ron"),
        &SubmoduleDefinition::SubmoduleV1(submodule.definition.clone()),
    )?;
    save_module_tree(fs, dir, &submodule.tree).map_err(Box::new)?;

    Ok(())
}
