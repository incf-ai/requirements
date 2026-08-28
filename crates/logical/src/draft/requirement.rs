use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use disk::{AttachmentReferenceKind, DependencyReferenceKind, TestReferenceKind};

use crate::pool::{AddPoolFileError, add_pool_file};

/// A requirement stage, freely editable. See `crates/logical/README.md`'s
/// data model section: `test: Option<T>`/`tests: Option<NonEmptyVec<T>>`
/// and `attachment`/`attachments` collapse into single `Vec`s here (decided
/// in "Decisions made" #1), and `attachments`/`attachment_refs` mirror
/// `disk`'s local-attachments-vs-declared-references split.
#[derive(Debug, Clone)]
pub struct RequirementDraft {
    pub title: String,
    pub requirement_text: String,
    pub requirement_guidance: Option<String>,
    pub test_guidance: Option<String>,
    pub tests: Vec<TestReferenceKind>,
    pub dependencies: Vec<DependencyReferenceKind>,
    /// Files physically local to this requirement's own `attachments/`.
    pub attachments: BTreeSet<PathBuf>,
    /// This requirement's `attachment`/`attachments` field, collapsed.
    pub attachment_refs: Vec<AttachmentReferenceKind>,
    pub include_attachments_in_commit: bool,
    /// The newest git commit touching this stage's folder, as of the last
    /// time this draft was imported from disk (`disk::RequirementOnDisk::commit`)
    /// — `None` for a requirement that only ever existed in-memory (never
    /// loaded from, or saved and reloaded from, an actual git repository).
    /// `is_requirement_met` needs this to tell a current result from a
    /// historical one — see `crates/logical/README.md`'s "Requirement-met
    /// semantics." Not re-derived by `validate()` (no filesystem/git access
    /// happens there for this) — only `import_project` sets it.
    pub commit: Option<String>,
}

impl RequirementDraft {
    pub fn new(title: impl Into<String>) -> Self {
        RequirementDraft {
            title: title.into(),
            requirement_text: String::new(),
            requirement_guidance: None,
            test_guidance: None,
            tests: Vec::new(),
            dependencies: Vec::new(),
            attachments: BTreeSet::new(),
            attachment_refs: Vec::new(),
            include_attachments_in_commit: true,
            commit: None,
        }
    }

    pub fn add_attachment(&mut self, path: &Path) -> Result<(), AddPoolFileError> {
        add_pool_file(&mut self.attachments, path)
    }

    pub fn remove_attachment(&mut self, path: &Path) -> bool {
        self.attachments.remove(path)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn new_starts_empty_with_attachments_included_in_commit() {
        let requirement = RequirementDraft::new("Title");
        assert_eq!(requirement.title, "Title");
        assert!(requirement.tests.is_empty());
        assert!(requirement.dependencies.is_empty());
        assert!(requirement.attachments.is_empty());
        assert!(requirement.include_attachments_in_commit);
        assert_eq!(requirement.commit, None);
    }

    #[test]
    fn add_and_remove_attachment_round_trip() {
        let mut requirement = RequirementDraft::new("Title");
        requirement.add_attachment(Path::new("notes.txt")).unwrap();
        assert!(requirement.attachments.contains(Path::new("notes.txt")));
        assert!(requirement.remove_attachment(Path::new("notes.txt")));
        assert!(requirement.attachments.is_empty());
    }

    #[test]
    fn remove_attachment_is_false_when_absent() {
        let mut requirement = RequirementDraft::new("Title");
        assert!(!requirement.remove_attachment(Path::new("notes.txt")));
    }
}
