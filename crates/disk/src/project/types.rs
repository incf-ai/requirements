use serde::{Deserialize, Serialize};

use crate::module::ModuleTree;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ProjectDefinition {
    RootV1(RootV1),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RootV1 {
    pub name: String,
}

/// A fully loaded project: the parsed `project.ron` plus its
/// requirements/tests/results/modules tree.
#[derive(Debug, Clone)]
pub struct ProjectOnDisk {
    pub definition: RootV1,
    pub tree: ModuleTree,
}
