mod attachments;
mod util;

pub mod module;
pub mod project;
pub mod requirement;
pub mod result;
pub mod test;

pub use attachments::AttachmentFile;
pub use util::EntryName;
pub use module::{
    ModuleTree, SubmoduleDefinition, SubmoduleOnDisk, SubmoduleV1, load_submodule, save_submodule,
};
pub use project::{ProjectDefinition, ProjectOnDisk, RootV1, load_project, save_project};
pub use requirement::{
    DependencyReferenceKind, LocalGitReference, ReferencePath, RequirementDefinition,
    RequirementDefinitionV1, RequirementOnDisk, TestReferenceKind, load_requirement_stage,
    save_requirement_stage,
};
pub use result::{ResultDefinition, ResultOnDisk, ResultsV1, StatusV1, load_result, save_result};
pub use test::{ResultKindV1, TestDefinition, TestOnDisk, TestV1, load_test, save_test};
