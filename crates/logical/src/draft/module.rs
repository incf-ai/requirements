use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use disk::EntryName;
use thiserror::Error;

use crate::draft::requirement::RequirementDraft;
use crate::draft::test::TestDraft;
use crate::pool::{AddPoolFileError, add_pool_file};
use crate::sanitize::{InvalidNameError, sanitize_entry_name};

/// The `attachments/`, `templates/`, `requirements/`, `tests/`, and
/// `modules/` children shared by both the project root and every submodule —
/// see `crates/logical/README.md`'s data model section. Results are not a
/// direct child here — each lives nested under its owning
/// `RequirementDraft.results`.
#[derive(Debug, Clone, Default)]
pub struct ModuleDraft {
    pub attachments: BTreeSet<PathBuf>,
    pub templates: BTreeSet<PathBuf>,
    pub requirements: BTreeMap<EntryName, RequirementDraft>,
    pub tests: BTreeMap<EntryName, TestDraft>,
    pub modules: BTreeMap<EntryName, ModuleDraft>,
}

/// One error type shared by every `add_<named child>` operation on
/// `ModuleDraft` — same shape regardless of which child collection, per
/// the operations catalog: "every `add_*` only fails if the name is
/// already taken in that exact map."
#[derive(Debug, Error)]
pub enum AddNamedChildError {
    #[error("invalid name: {0}")]
    InvalidName(#[from] InvalidNameError),
    #[error("`{0}` already exists")]
    AlreadyExists(EntryName),
    #[error("{0} must not be empty")]
    EmptyText(&'static str),
}

/// Mirrors `AddNamedChildError`'s "one error type shared across every
/// sibling operation" reasoning, but for `update_requirement`/
/// `update_test`: the name isn't changing (so no `InvalidName`/
/// `AlreadyExists`), just whether the entry exists to update, plus the
/// same main-text-must-not-be-empty rule `add_requirement`/`add_test`
/// enforce — see those methods' own doc comments.
#[derive(Debug, Error)]
pub enum UpdateNamedChildError {
    #[error("no entry with that name exists yet — use add instead")]
    NotFound,
    #[error("{0} must not be empty")]
    EmptyText(&'static str),
}

/// Turns a sanitized entry name like `foo_bar_baz` into a human display
/// title, `Foo Bar Baz` — underscores and whitespace become word
/// boundaries (a name may already contain spaces, e.g. a pasted `Foo copy
/// 2` or a `<date> <test name>` result identifier), each word capitalized.
/// Used to autopopulate an empty title when adding a requirement/test/
/// result, and (via `logical::draft::title_case_from_name`) to regenerate a
/// title after a rename/recreate/duplicate.
pub fn title_case_from_name(name: &str) -> String {
    name.split(|c: char| c == '_' || c.is_whitespace())
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn add_named<T>(
    map: &mut BTreeMap<EntryName, T>,
    name: &str,
    value: T,
) -> Result<(), AddNamedChildError> {
    let name = sanitize_entry_name(name)?;
    if map.contains_key(&name) {
        return Err(AddNamedChildError::AlreadyExists(name));
    }
    map.insert(name, value);
    Ok(())
}

impl ModuleDraft {
    pub fn add_module(&mut self, name: &str) -> Result<(), AddNamedChildError> {
        add_named(&mut self.modules, name, ModuleDraft::default())
    }

    pub fn remove_module(&mut self, name: &str) -> Option<ModuleDraft> {
        self.modules.remove(&EntryName(name.to_string()))
    }

    /// Refuses an empty `requirement_text` (there's nothing to check
    /// against) and autopopulates an empty `title` from `name` (title
    /// case, underscores removed) rather than saving a blank one.
    pub fn add_requirement(
        &mut self,
        name: &str,
        mut requirement: RequirementDraft,
    ) -> Result<(), AddNamedChildError> {
        if requirement.requirement_text.trim().is_empty() {
            return Err(AddNamedChildError::EmptyText("requirement text"));
        }
        if requirement.title.trim().is_empty() {
            requirement.title = title_case_from_name(name);
        }
        add_named(&mut self.requirements, name, requirement)
    }

    pub fn remove_requirement(&mut self, name: &str) -> Option<RequirementDraft> {
        self.requirements.remove(&EntryName(name.to_string()))
    }

    /// See `add_requirement`'s doc comment — same empty-text refusal and
    /// title autopopulation apply to editing an existing requirement, not
    /// just creating one.
    pub fn update_requirement(
        &mut self,
        name: &EntryName,
        mut requirement: RequirementDraft,
    ) -> Result<(), UpdateNamedChildError> {
        if requirement.requirement_text.trim().is_empty() {
            return Err(UpdateNamedChildError::EmptyText("requirement text"));
        }
        if requirement.title.trim().is_empty() {
            requirement.title = title_case_from_name(name.as_str());
        }
        match self.requirements.entry(name.clone()) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                e.insert(requirement);
                Ok(())
            }
            std::collections::btree_map::Entry::Vacant(_) => Err(UpdateNamedChildError::NotFound),
        }
    }

    /// See `add_requirement`'s doc comment — same empty-text refusal and
    /// title autopopulation, against `test_text` instead of
    /// `requirement_text`.
    pub fn add_test(&mut self, name: &str, mut test: TestDraft) -> Result<(), AddNamedChildError> {
        if test.test_text.trim().is_empty() {
            return Err(AddNamedChildError::EmptyText("test text"));
        }
        if test.title.trim().is_empty() {
            test.title = title_case_from_name(name);
        }
        add_named(&mut self.tests, name, test)
    }

    pub fn remove_test(&mut self, name: &str) -> Option<TestDraft> {
        self.tests.remove(&EntryName(name.to_string()))
    }

    /// See `update_requirement`'s doc comment — same idea, against
    /// `test_text` instead of `requirement_text`.
    pub fn update_test(&mut self, name: &EntryName, mut test: TestDraft) -> Result<(), UpdateNamedChildError> {
        if test.test_text.trim().is_empty() {
            return Err(UpdateNamedChildError::EmptyText("test text"));
        }
        if test.title.trim().is_empty() {
            test.title = title_case_from_name(name.as_str());
        }
        match self.tests.entry(name.clone()) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                e.insert(test);
                Ok(())
            }
            std::collections::btree_map::Entry::Vacant(_) => Err(UpdateNamedChildError::NotFound),
        }
    }

    pub fn add_attachment(&mut self, path: &Path) -> Result<(), AddPoolFileError> {
        add_pool_file(&mut self.attachments, path)
    }

    pub fn remove_attachment(&mut self, path: &Path) -> bool {
        self.attachments.remove(path)
    }

    pub fn add_template(&mut self, path: &Path) -> Result<(), AddPoolFileError> {
        add_pool_file(&mut self.templates, path)
    }

    pub fn remove_template(&mut self, path: &Path) -> bool {
        self.templates.remove(path)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use disk::ResultKindV1;

    fn requirement_draft(title: &str) -> RequirementDraft {
        let mut requirement = RequirementDraft::new(title);
        requirement.requirement_text = "Text".to_string();
        requirement
    }

    fn test_draft(title: &str) -> TestDraft {
        let mut test = TestDraft::new(title, ResultKindV1::FreeForm);
        test.test_text = "Text".to_string();
        test
    }

    #[test]
    fn add_module_then_remove_round_trips() {
        let mut module = ModuleDraft::default();
        module.add_module("embeddings").unwrap();
        assert!(
            module
                .modules
                .contains_key(&EntryName("embeddings".to_string()))
        );
        assert!(module.remove_module("embeddings").is_some());
        assert!(module.modules.is_empty());
    }

    #[test]
    fn remove_module_is_none_when_absent() {
        let mut module = ModuleDraft::default();
        assert!(module.remove_module("embeddings").is_none());
    }

    #[test]
    fn add_module_rejects_an_invalid_name() {
        let mut module = ModuleDraft::default();
        let err = module.add_module("").unwrap_err();
        assert!(matches!(err, AddNamedChildError::InvalidName(_)));
    }

    #[test]
    fn add_module_rejects_a_duplicate_name() {
        let mut module = ModuleDraft::default();
        module.add_module("embeddings").unwrap();
        let err = module.add_module("embeddings").unwrap_err();
        assert!(matches!(err, AddNamedChildError::AlreadyExists(_)));
    }

    #[test]
    fn add_requirement_then_remove_round_trips() {
        let mut module = ModuleDraft::default();
        module
            .add_requirement("definition", requirement_draft("Definition"))
            .unwrap();
        assert!(module.remove_requirement("definition").is_some());
    }

    #[test]
    fn add_test_then_remove_round_trips() {
        let mut module = ModuleDraft::default();
        module
            .add_test("generic_test", test_draft("Generic Test"))
            .unwrap();
        assert!(module.remove_test("generic_test").is_some());
    }

    #[test]
    fn add_requirement_rejects_empty_requirement_text() {
        let mut module = ModuleDraft::default();
        let err = module
            .add_requirement("definition", RequirementDraft::new("Definition"))
            .unwrap_err();
        assert!(matches!(err, AddNamedChildError::EmptyText("requirement text")));
        assert!(module.requirements.is_empty());
    }

    #[test]
    fn add_requirement_autopopulates_an_empty_title_from_the_name() {
        let mut module = ModuleDraft::default();
        module
            .add_requirement("some_definition", requirement_draft(""))
            .unwrap();
        assert_eq!(
            module
                .requirements
                .get(&EntryName("some_definition".to_string()))
                .unwrap()
                .title,
            "Some Definition"
        );
    }

    #[test]
    fn add_test_rejects_empty_test_text() {
        let mut module = ModuleDraft::default();
        let err = module
            .add_test(
                "generic_test",
                TestDraft::new("Generic Test", ResultKindV1::FreeForm),
            )
            .unwrap_err();
        assert!(matches!(err, AddNamedChildError::EmptyText("test text")));
        assert!(module.tests.is_empty());
    }

    #[test]
    fn add_test_autopopulates_an_empty_title_from_the_name() {
        let mut module = ModuleDraft::default();
        module
            .add_test("some_test", test_draft(""))
            .unwrap();
        assert_eq!(
            module
                .tests
                .get(&EntryName("some_test".to_string()))
                .unwrap()
                .title,
            "Some Test"
        );
    }

    #[test]
    fn title_case_from_name_splits_on_underscores() {
        assert_eq!(title_case_from_name("foo_bar_baz"), "Foo Bar Baz");
    }

    #[test]
    fn title_case_from_name_splits_on_whitespace() {
        assert_eq!(title_case_from_name("2026-01-31 demonstration"), "2026-01-31 Demonstration");
        assert_eq!(title_case_from_name("foo copy 2"), "Foo Copy 2");
    }

    #[test]
    fn title_case_from_name_splits_on_mixed_underscores_and_whitespace() {
        assert_eq!(title_case_from_name("foo_bar baz"), "Foo Bar Baz");
    }

    #[test]
    fn title_case_from_name_collapses_repeated_separators() {
        assert_eq!(title_case_from_name("foo__bar"), "Foo Bar");
        assert_eq!(title_case_from_name("foo  bar"), "Foo Bar");
        assert_eq!(title_case_from_name("_foo_"), "Foo");
    }

    #[test]
    fn add_requirement_rejects_a_duplicate_name() {
        let mut module = ModuleDraft::default();
        module
            .add_requirement("definition", requirement_draft("Definition"))
            .unwrap();
        let err = module
            .add_requirement("definition", requirement_draft("Definition"))
            .unwrap_err();
        assert!(matches!(err, AddNamedChildError::AlreadyExists(_)));
    }

    #[test]
    fn add_test_rejects_a_duplicate_name() {
        let mut module = ModuleDraft::default();
        module
            .add_test("generic_test", test_draft("Generic Test"))
            .unwrap();
        let err = module
            .add_test("generic_test", test_draft("Generic Test"))
            .unwrap_err();
        assert!(matches!(err, AddNamedChildError::AlreadyExists(_)));
    }

    #[test]
    fn add_attachment_then_remove_round_trips() {
        let mut module = ModuleDraft::default();
        module.add_attachment(Path::new("glossary.md")).unwrap();
        assert!(module.remove_attachment(Path::new("glossary.md")));
    }

    #[test]
    fn add_template_then_remove_round_trips() {
        let mut module = ModuleDraft::default();
        module.add_template(Path::new("summary.txt")).unwrap();
        assert!(module.remove_template(Path::new("summary.txt")));
    }
}
