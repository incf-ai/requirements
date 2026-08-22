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

#[cfg(test)]
mod test {
    use super::*;
    use crate::requirement::types::{RequirementDefinitionV1, RequirementOnDisk};
    use crate::result::types::{ResultOnDisk, ResultsV1};
    use crate::requirement::types::ReferencePath;
    use crate::test::types::{ResultKindV1, TestOnDisk, TestV1};
    use crate::module::types::SubmoduleOnDisk;
    use crate::util::EntryName;
    use syscalls::{FaultInjectingFilesystem, StdFilesystem};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "disk-module-ops-{name}-{}-{}",
            std::process::id(),
            line!()
        ))
    }

    /// Builds a `dir` with all four required subdirectories present, except
    /// for `skip`, which is left entirely absent.
    fn dir_with_all_but(name: &str, skip: &str) -> PathBuf {
        let dir = temp_dir(name);
        for sub in ["requirements", "tests", "results", "modules"] {
            if sub != skip {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
        }
        dir
    }

    #[test]
    fn load_module_tree_reports_missing_tests_dir() {
        let dir = dir_with_all_but("missing-tests", "tests");
        let err = load_module_tree(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err, LoadModuleTreeError::Tests(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_module_tree_reports_missing_results_dir() {
        let dir = dir_with_all_but("missing-results", "results");
        let err = load_module_tree(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err, LoadModuleTreeError::Results(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_module_tree_reports_missing_modules_dir() {
        let dir = dir_with_all_but("missing-modules", "modules");
        let err = load_module_tree(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err, LoadModuleTreeError::Modules(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn minimal_requirement(name: &str) -> RequirementOnDisk {
        RequirementOnDisk {
            name: EntryName(name.to_string()),
            definition: RequirementDefinitionV1 {
                title: "Title".to_string(),
                test: None,
                tests: None,
                dependency: None,
                dependencies: None,
            },
            requirement_text: String::new(),
            requirement_guidance: None,
            test_guidance: None,
            attachments: Vec::new(),
        }
    }

    fn minimal_test(name: &str) -> TestOnDisk {
        TestOnDisk {
            name: EntryName(name.to_string()),
            definition: TestV1 {
                title: "Title".to_string(),
                result_kind: ResultKindV1::FreeForm,
            },
            test_text: String::new(),
            attachments: Vec::new(),
            template: Vec::new(),
        }
    }

    fn minimal_result(name: &str) -> ResultOnDisk {
        ResultOnDisk {
            name: EntryName(name.to_string()),
            definition: ResultsV1 {
                title: "Title".to_string(),
                path: ReferencePath("requirements/definition".to_string()),
                commit: "abc".to_string(),
                status: None,
            },
            attachments: Vec::new(),
        }
    }

    fn minimal_submodule(name: &str) -> SubmoduleOnDisk {
        SubmoduleOnDisk {
            name: EntryName(name.to_string()),
            definition: crate::module::types::SubmoduleV1 {
                name: name.to_string(),
            },
            tree: ModuleTree::default(),
        }
    }

    #[test]
    fn save_module_tree_reports_io_errors_creating_each_subdir() {
        for sub in ["requirements", "tests", "results", "modules"] {
            let dir = temp_dir(&format!("create-dir-{sub}"));
            let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
            fs.inject(dir.join(sub), io::ErrorKind::PermissionDenied);

            let err = save_module_tree(&fs, &dir, &ModuleTree::default()).unwrap_err();
            assert!(
                matches!(err, SaveModuleTreeError::CreateDir { .. }),
                "expected CreateDir failure for {sub}"
            );
        }
    }

    #[test]
    fn save_module_tree_reports_a_failing_requirement() {
        let dir = temp_dir("failing-requirement");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("requirements").join("foo"), io::ErrorKind::PermissionDenied);

        let tree = ModuleTree {
            requirements: vec![minimal_requirement("foo")],
            ..Default::default()
        };
        let err = save_module_tree(&fs, &dir, &tree).unwrap_err();
        assert!(matches!(err, SaveModuleTreeError::Requirement { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_module_tree_reports_a_failing_test() {
        let dir = temp_dir("failing-test");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("tests").join("foo"), io::ErrorKind::PermissionDenied);

        let tree = ModuleTree {
            tests: vec![minimal_test("foo")],
            ..Default::default()
        };
        let err = save_module_tree(&fs, &dir, &tree).unwrap_err();
        assert!(matches!(err, SaveModuleTreeError::Test { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_module_tree_reports_a_failing_result() {
        let dir = temp_dir("failing-result");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("results").join("foo"), io::ErrorKind::PermissionDenied);

        let tree = ModuleTree {
            results: vec![minimal_result("foo")],
            ..Default::default()
        };
        let err = save_module_tree(&fs, &dir, &tree).unwrap_err();
        assert!(matches!(err, SaveModuleTreeError::Result { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_module_tree_reports_a_failing_module() {
        let dir = temp_dir("failing-module");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(dir.join("modules").join("foo"), io::ErrorKind::PermissionDenied);

        let tree = ModuleTree {
            modules: vec![minimal_submodule("foo")],
            ..Default::default()
        };
        let err = save_module_tree(&fs, &dir, &tree).unwrap_err();
        assert!(matches!(err, SaveModuleTreeError::Module { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }
}
