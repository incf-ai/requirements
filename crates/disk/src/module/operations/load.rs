use std::path::Path;

use syscalls::Filesystem;
use thiserror::Error;

use crate::module::operations::{LoadModuleTreeError, load_module_tree};
use crate::module::types::{SubmoduleDefinition, SubmoduleOnDisk};
use crate::util::{EntryName, LoadRonError, load_ron};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to load submodule.ron: {0}")]
    Definition(#[from] LoadRonError),
    #[error("failed to load module tree: {0}")]
    Tree(#[from] Box<LoadModuleTreeError>),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

pub fn load_submodule(fs: &dyn Filesystem, dir: &Path) -> Result<SubmoduleOnDisk, Error> {
    load_submodule_inner(fs, dir).map_err(Error)
}

fn load_submodule_inner(fs: &dyn Filesystem, dir: &Path) -> Result<SubmoduleOnDisk, ErrorKind> {
    let SubmoduleDefinition::SubmoduleV1(definition) = load_ron(fs, &dir.join("submodule.ron"))?;
    let tree = load_module_tree(fs, dir).map_err(Box::new)?;

    Ok(SubmoduleOnDisk {
        name: EntryName::of(dir),
        definition,
        tree,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::util::LoadNamedChildrenError;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn loads_the_embeddings_submodule_from_the_sample_project() -> Result<(), Error> {
        let dir = sample_project_dir().join("modules/embeddings");
        let submodule = load_submodule(&StdFilesystem, &dir)?;

        assert_eq!(submodule.definition.name, "Embeddings");
        assert!(submodule.tree.requirements.is_empty());
        assert!(submodule.tree.tests.is_empty());
        assert!(submodule.tree.results.is_empty());
        assert!(submodule.tree.modules.is_empty());

        Ok(())
    }

    /// Builds a minimal valid `modules/<name>/` folder in a fresh tempdir.
    fn valid_submodule_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "disk-submodule-load-{name}-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join("requirements")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::create_dir_all(dir.join("results")).unwrap();
        std::fs::create_dir_all(dir.join("modules")).unwrap();
        std::fs::write(dir.join("submodule.ron"), "SubmoduleV1(name: \"Name\")").unwrap();
        dir
    }

    #[test]
    fn missing_submodule_ron_is_reported() {
        let dir = valid_submodule_dir("missing-ron");
        std::fs::remove_file(dir.join("submodule.ron")).unwrap();

        let err = load_submodule(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(
            err.0,
            ErrorKind::Definition(LoadRonError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_requirements_dir_is_reported() {
        let dir = valid_submodule_dir("missing-requirements");
        std::fs::remove_dir(dir.join("requirements")).unwrap();

        let err = load_submodule(&StdFilesystem, &dir).unwrap_err();
        let ErrorKind::Tree(tree) = err.0 else {
            panic!("expected ErrorKind::Tree");
        };
        assert!(matches!(
            *tree,
            LoadModuleTreeError::Requirements(LoadNamedChildrenError::Missing { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
