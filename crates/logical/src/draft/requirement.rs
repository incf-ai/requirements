use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use disk::{AttachmentReferenceKind, DependencyReferenceKind, EntryName, TestReferenceKind};

use crate::draft::module::{AddNamedChildError, UpdateNamedChildError, title_case_from_name};
use crate::draft::result::ResultDraft;
use crate::pool::{AddPoolFileError, add_pool_file};

/// A requirement stage, freely editable. See `crates/logical/README.md`'s
/// data model section: `test: Option<T>`/`tests: Option<NonEmptyVec<T>>`
/// and `attachment`/`attachments` collapse into single `Vec`s here (decided
/// in "Decisions made" #1), and `attachments`/`attachment_refs` mirror
/// `disk`'s local-attachments-vs-declared-references split. `results` are
/// owned here, not at the module level: a result's requirement is
/// structural (it lives nested under it), not a field to keep in sync — see
/// `disk::RequirementOnDisk::results`.
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
    pub results: BTreeMap<EntryName, ResultDraft>,
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
            results: BTreeMap::new(),
            commit: None,
        }
    }

    pub fn add_attachment(&mut self, path: &Path) -> Result<(), AddPoolFileError> {
        add_pool_file(&mut self.attachments, path)
    }

    pub fn remove_attachment(&mut self, path: &Path) -> bool {
        self.attachments.remove(path)
    }

    /// See `ModuleDraft::add_requirement`'s doc comment — same title
    /// autopopulation, from the result's own name rather than its owning
    /// requirement's.
    pub fn add_result(&mut self, name: &str, mut result: ResultDraft) -> Result<(), AddNamedChildError> {
        if result.title.trim().is_empty() {
            result.title = title_case_from_name(name);
        }
        crate::draft::module::add_named(&mut self.results, name, result)
    }

    pub fn remove_result(&mut self, name: &str) -> Option<ResultDraft> {
        self.results.remove(&EntryName(name.to_string()))
    }

    /// See `ModuleDraft::update_requirement`'s doc comment — same title
    /// autopopulation apply to editing an existing result, not just
    /// creating one. Unlike requirement/test text, a result has no
    /// "must not be empty" field to enforce here.
    pub fn update_result(&mut self, name: &EntryName, mut result: ResultDraft) -> Result<(), UpdateNamedChildError> {
        if result.title.trim().is_empty() {
            result.title = title_case_from_name(name.as_str());
        }
        match self.results.entry(name.clone()) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                e.insert(result);
                Ok(())
            }
            std::collections::btree_map::Entry::Vacant(_) => Err(UpdateNamedChildError::NotFound),
        }
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

    fn minimal_result_draft() -> ResultDraft {
        ResultDraft::new(
            "Definition",
            "abc",
            disk::ReferencePath("tests/generic_test".to_string()),
            "def",
        )
    }

    #[test]
    fn add_result_then_remove_round_trips() {
        let mut requirement = RequirementDraft::new("Title");
        requirement
            .add_result("definition", minimal_result_draft())
            .unwrap();
        assert!(requirement.remove_result("definition").is_some());
    }

    #[test]
    fn add_result_rejects_a_duplicate_name() {
        let mut requirement = RequirementDraft::new("Title");
        requirement
            .add_result("definition", minimal_result_draft())
            .unwrap();
        let err = requirement
            .add_result("definition", minimal_result_draft())
            .unwrap_err();
        assert!(matches!(err, AddNamedChildError::AlreadyExists(_)));
    }

    #[test]
    fn remove_result_is_none_when_absent() {
        let mut requirement = RequirementDraft::new("Title");
        assert!(requirement.remove_result("definition").is_none());
    }

    fn blank_title_result_draft() -> ResultDraft {
        ResultDraft::new(
            "",
            "abc",
            disk::ReferencePath("tests/generic_test".to_string()),
            "def",
        )
    }

    #[test]
    fn add_result_autopopulates_an_empty_title_from_the_name() {
        let mut requirement = RequirementDraft::new("Title");
        requirement
            .add_result("some_result", blank_title_result_draft())
            .unwrap();
        assert_eq!(
            requirement
                .results
                .get(&EntryName("some_result".to_string()))
                .unwrap()
                .title,
            "Some Result"
        );
    }

    #[test]
    fn update_result_replaces_content_and_autopopulates_an_empty_title() {
        let mut requirement = RequirementDraft::new("Title");
        requirement
            .add_result("some_result", minimal_result_draft())
            .unwrap();
        requirement
            .update_result(&EntryName("some_result".to_string()), blank_title_result_draft())
            .unwrap();
        assert_eq!(
            requirement
                .results
                .get(&EntryName("some_result".to_string()))
                .unwrap()
                .title,
            "Some Result"
        );
    }

    #[test]
    fn update_result_is_not_found_when_absent() {
        let mut requirement = RequirementDraft::new("Title");
        let err = requirement
            .update_result(&EntryName("definition".to_string()), minimal_result_draft())
            .unwrap_err();
        assert!(matches!(err, UpdateNamedChildError::NotFound));
    }
}
