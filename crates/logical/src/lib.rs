pub mod convert;
pub mod draft;
mod lookup;
mod path;
mod pool;
mod reference_repair;
mod sanitize;
#[cfg(test)]
mod test_support;
pub mod validate;
mod validated;

pub use path::{LogicalPath, ResultPath, resolve_reference_path};
pub use pool::AddPoolFileError;
pub use reference_repair::{
    ReferenceAction, ReferenceRepairError, ReferenceSite, ReferenceSiteKind, ReferenceTarget,
    apply_reference_actions, find_references,
};
pub use sanitize::{InvalidNameError, InvalidPathError};
pub use validated::{
    RequirementResult, TestUnmetReason, UnmetReason, UnsatisfiedTest, ValidatedProject, results_for_requirement,
};
