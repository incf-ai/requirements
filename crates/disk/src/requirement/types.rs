use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::attachments::AttachmentFile;
use crate::util::EntryName;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum RequirementDefinition {
    RequirementDefinitionV1(RequirementDefinitionV1),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde_with::skip_serializing_none]
pub struct RequirementDefinitionV1 {
    pub title: String,
    pub test: Option<TestReferenceKind>,
    pub tests: Option<nunny::Vec<TestReferenceKind>>,
    pub dependency: Option<DependencyReferenceKind>,
    pub dependencies: Option<nunny::Vec<DependencyReferenceKind>>,
}

#[derive(Debug, Error)]
pub(crate) enum ValidateRequirementDefinitionError {
    #[error("sets both `{singular}` and `{plural}` — use only one")]
    AmbiguousField {
        singular: &'static str,
        plural: &'static str,
    },
}

impl RequirementDefinitionV1 {
    pub(crate) fn validate(&self) -> Result<(), ValidateRequirementDefinitionError> {
        if self.test.is_some() && self.tests.is_some() {
            return Err(ValidateRequirementDefinitionError::AmbiguousField {
                singular: "test",
                plural: "tests",
            });
        }
        if self.dependency.is_some() && self.dependencies.is_some() {
            return Err(ValidateRequirementDefinitionError::AmbiguousField {
                singular: "dependency",
                plural: "dependencies",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TestReferenceKind {
    TestReferenceV1(LocalGitReference),
}

/// A leading `/` means the path is relative to the project root; no leading
/// slash means relative to the current module's own root.
///
/// Deliberately `String`, not `PathBuf`: `PathBuf::join` treats a leading
/// `/` as an OS-absolute path and silently *discards* whatever it's joined
/// onto — the opposite of what this leading-slash convention means here.
/// Resolving a `ReferencePath` against an actual loaded tree must apply that
/// convention explicitly rather than delegating to `Path`'s own semantics.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReferencePath(pub String);

impl std::fmt::Display for ReferencePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A reference path pinned to a specific commit.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalGitReference {
    pub path: ReferencePath,
    pub commit: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DependencyReferenceKind {
    RequirementReferenceV1(LocalGitReference),
    Submodules,
}

/// A fully loaded `requirements/<stage>/` folder: the parsed `requirement.ron`
/// plus its sibling typst files and attachments.
#[derive(Debug, Clone)]
pub struct RequirementOnDisk {
    /// This stage's directory name (e.g. `definition`), not to be confused
    /// with `definition.title`, a separate human-readable display title.
    pub name: EntryName,
    pub definition: RequirementDefinitionV1,
    pub requirement_text: String,
    pub requirement_guidance: Option<String>,
    pub test_guidance: Option<String>,
    pub attachments: Vec<AttachmentFile>,
}
