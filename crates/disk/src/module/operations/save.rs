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

#[cfg(test)]
mod test {
    use super::*;
    use crate::module::types::{ModuleTree, SubmoduleV1};
    use crate::util::EntryName;
    use syscalls::{FaultInjectingFilesystem, StdFilesystem};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "disk-submodule-save-{name}-{}-{}",
            std::process::id(),
            line!()
        ))
    }

    fn minimal_submodule() -> SubmoduleOnDisk {
        SubmoduleOnDisk {
            name: EntryName("setup".to_string()),
            definition: SubmoduleV1 {
                name: "Setup".to_string(),
            },
            tree: ModuleTree::default(),
        }
    }

    #[test]
    fn reports_io_errors_creating_the_directory() {
        let dir = temp_dir("create-dir-io");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(&dir, io::ErrorKind::PermissionDenied);

        let err = save_submodule(&fs, &dir, &minimal_submodule()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::CreateDir { .. }));
    }

    #[test]
    fn reports_io_errors_saving_submodule_ron() {
        let dir = temp_dir("definition-io");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("submodule.ron"), io::ErrorKind::PermissionDenied);

        let err = save_submodule(&fs, &dir, &minimal_submodule()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Definition(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_a_failing_module_tree() {
        let dir = temp_dir("tree-io");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("requirements"), io::ErrorKind::PermissionDenied);

        let err = save_submodule(&fs, &dir, &minimal_submodule()).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Tree(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn error_messages_are_readable() {
        let dir = temp_dir("message");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("submodule.ron"), io::ErrorKind::PermissionDenied);

        let err = save_submodule(&fs, &dir, &minimal_submodule()).unwrap_err();
        assert!(err.to_string().contains("failed to save submodule.ron"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
