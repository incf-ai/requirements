use serde::{Deserialize, Serialize};

use crate::requirement::types::RequirementOnDisk;
use crate::result::types::ResultOnDisk;
use crate::test::types::TestOnDisk;
use crate::util::EntryName;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SubmoduleDefinition {
    SubmoduleV1(SubmoduleV1),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubmoduleV1 {
    pub name: String,
}

/// The `requirements/`, `tests/`, `results/`, and `modules/` children shared
/// by both the project root and every submodule, each keyed by directory
/// name.
#[derive(Debug, Clone, Default)]
pub struct ModuleTree {
    pub requirements: Vec<RequirementOnDisk>,
    pub tests: Vec<TestOnDisk>,
    pub results: Vec<ResultOnDisk>,
    pub modules: Vec<SubmoduleOnDisk>,
}

/// A fully loaded `modules/<name>/` folder: the parsed `submodule.ron` plus
/// its requirements/tests/results/nested-modules tree.
#[derive(Debug, Clone)]
pub struct SubmoduleOnDisk {
    /// This submodule's directory name (e.g. `setup`), not to be confused
    /// with `definition.name`, a separate human-readable display name.
    pub name: EntryName,
    pub definition: SubmoduleV1,
    pub tree: ModuleTree,
}
