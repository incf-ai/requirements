use std::io;
use std::path::{Path, PathBuf};

use syscalls::Filesystem;
use thiserror::Error;

use crate::module::types::ModuleTree;
use crate::requirement::operations::load::Error as LoadRequirementStageError;
use crate::requirement::operations::save::Error as SaveRequirementStageError;
use crate::result::operations::load::Error as LoadResultError;
use crate::result::operations::save::Error as SaveResultError;
use crate::test::operations::load::Error as LoadTestError;
use crate::test::operations::save::Error as SaveTestError;
use crate::util::{LoadNamedChildrenError, load_named_children};

pub mod load;
pub mod save;

pub use load::load_submodule;
pub use save::save_submodule;

use load::Error as LoadSubmoduleError;
use save::Error as SaveSubmoduleError;

#[derive(Debug, Error)]
pub(crate) enum LoadModuleTreeError {
    #[error("failed to load requirements: {0}")]
    Requirements(#[from] LoadNamedChildrenError<LoadRequirementStageError>),
    #[error("failed to load tests: {0}")]
    Tests(#[from] LoadNamedChildrenError<LoadTestError>),
    #[error("failed to load results: {0}")]
    Results(#[from] LoadNamedChildrenError<LoadResultError>),
    #[error("failed to load modules: {0}")]
    Modules(#[from] LoadNamedChildrenError<LoadSubmoduleError>),
}

pub(crate) fn load_module_tree(
    fs: &dyn Filesystem,
    dir: &Path,
) -> Result<ModuleTree, LoadModuleTreeError> {
    let requirements = load_named_children(
        fs,
        &dir.join("requirements"),
        crate::requirement::operations::load_requirement_stage,
    )?;
    let tests = load_named_children(
        fs,
        &dir.join("tests"),
        crate::test::operations::load_test,
    )?;
    let results = load_named_children(
        fs,
        &dir.join("results"),
        crate::result::operations::load_result,
    )?;
    let modules = load_named_children(fs, &dir.join("modules"), load_submodule)?;

    Ok(ModuleTree {
        requirements,
        tests,
        results,
        modules,
    })
}

#[derive(Debug, Error)]
pub(crate) enum SaveModuleTreeError {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to save requirement '{name}': {source}")]
    Requirement {
        name: String,
        #[source]
        source: SaveRequirementStageError,
    },
    #[error("failed to save test '{name}': {source}")]
    Test {
        name: String,
        #[source]
        source: SaveTestError,
    },
    #[error("failed to save result '{name}': {source}")]
    Result {
        name: String,
        #[source]
        source: SaveResultError,
    },
    #[error("failed to save module '{name}': {source}")]
    Module {
        name: String,
        #[source]
        source: SaveSubmoduleError,
    },
}

pub(crate) fn save_module_tree(
    fs: &dyn Filesystem,
    dir: &Path,
    tree: &ModuleTree,
) -> Result<(), SaveModuleTreeError> {
    let requirements_dir = dir.join("requirements");
    fs.create_dir_all(&requirements_dir)
        .map_err(|source| SaveModuleTreeError::CreateDir {
            path: requirements_dir.clone(),
            source,
        })?;
    for requirement in &tree.requirements {
        crate::requirement::operations::save_requirement_stage(
            fs,
            &requirements_dir.join(&requirement.name),
            requirement,
        )
        .map_err(|source| SaveModuleTreeError::Requirement {
            name: requirement.name.to_string(),
            source,
        })?;
    }

    let tests_dir = dir.join("tests");
    fs.create_dir_all(&tests_dir)
        .map_err(|source| SaveModuleTreeError::CreateDir {
            path: tests_dir.clone(),
            source,
        })?;
    for test in &tree.tests {
        crate::test::operations::save_test(fs, &tests_dir.join(&test.name), test).map_err(
            |source| SaveModuleTreeError::Test {
                name: test.name.to_string(),
                source,
            },
        )?;
    }

    let results_dir = dir.join("results");
    fs.create_dir_all(&results_dir)
        .map_err(|source| SaveModuleTreeError::CreateDir {
            path: results_dir.clone(),
            source,
        })?;
    for result in &tree.results {
        crate::result::operations::save_result(fs, &results_dir.join(&result.name), result)
            .map_err(|source| SaveModuleTreeError::Result {
                name: result.name.to_string(),
                source,
            })?;
    }

    let modules_dir = dir.join("modules");
    fs.create_dir_all(&modules_dir)
        .map_err(|source| SaveModuleTreeError::CreateDir {
            path: modules_dir.clone(),
            source,
        })?;
    for submodule in &tree.modules {
        save_submodule(fs, &modules_dir.join(&submodule.name), submodule).map_err(|source| {
            SaveModuleTreeError::Module {
                name: submodule.name.to_string(),
                source,
            }
        })?;
    }

    Ok(())
}
