use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use disk::EntryName;
use thiserror::Error;

use crate::draft::requirement::RequirementDraft;
use crate::draft::result::ResultDraft;
use crate::draft::test::TestDraft;
use crate::pool::{AddPoolFileError, add_pool_file};
use crate::sanitize::{InvalidNameError, sanitize_entry_name};

/// The `attachments/`, `templates/`, `requirements/`, `tests/`, `results/`,
/// and `modules/` children shared by both the project root and every
/// submodule — see `crates/logical/README.md`'s data model section.
#[derive(Debug, Clone, Default)]
pub struct ModuleDraft {
    pub attachments: BTreeSet<PathBuf>,
    pub templates: BTreeSet<PathBuf>,
    pub requirements: BTreeMap<EntryName, RequirementDraft>,
    pub tests: BTreeMap<EntryName, TestDraft>,
    pub results: BTreeMap<EntryName, ResultDraft>,
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
}

fn add_named<T>(
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

    pub fn add_requirement(
        &mut self,
        name: &str,
        requirement: RequirementDraft,
    ) -> Result<(), AddNamedChildError> {
        add_named(&mut self.requirements, name, requirement)
    }

    pub fn remove_requirement(&mut self, name: &str) -> Option<RequirementDraft> {
        self.requirements.remove(&EntryName(name.to_string()))
    }

    pub fn add_test(&mut self, name: &str, test: TestDraft) -> Result<(), AddNamedChildError> {
        add_named(&mut self.tests, name, test)
    }

    pub fn remove_test(&mut self, name: &str) -> Option<TestDraft> {
        self.tests.remove(&EntryName(name.to_string()))
    }

    pub fn add_result(
        &mut self,
        name: &str,
        result: ResultDraft,
    ) -> Result<(), AddNamedChildError> {
        add_named(&mut self.results, name, result)
    }

    pub fn remove_result(&mut self, name: &str) -> Option<ResultDraft> {
        self.results.remove(&EntryName(name.to_string()))
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
            .add_requirement("definition", RequirementDraft::new("Definition"))
            .unwrap();
        assert!(module.remove_requirement("definition").is_some());
    }

    #[test]
    fn add_test_then_remove_round_trips() {
        let mut module = ModuleDraft::default();
        module
            .add_test(
                "generic_test",
                TestDraft::new("Generic Test", ResultKindV1::FreeForm),
            )
            .unwrap();
        assert!(module.remove_test("generic_test").is_some());
    }

    #[test]
    fn add_result_then_remove_round_trips() {
        let mut module = ModuleDraft::default();
        let result = crate::draft::result::ResultDraft::new(
            "Definition",
            disk::ReferencePath("requirements/definition".to_string()),
            "abc",
            disk::ReferencePath("tests/generic_test".to_string()),
            "def",
        );
        module.add_result("definition", result).unwrap();
        assert!(module.remove_result("definition").is_some());
    }

    fn minimal_result_draft() -> crate::draft::result::ResultDraft {
        crate::draft::result::ResultDraft::new(
            "Definition",
            disk::ReferencePath("requirements/definition".to_string()),
            "abc",
            disk::ReferencePath("tests/generic_test".to_string()),
            "def",
        )
    }

    #[test]
    fn add_requirement_rejects_a_duplicate_name() {
        let mut module = ModuleDraft::default();
        module
            .add_requirement("definition", RequirementDraft::new("Definition"))
            .unwrap();
        let err = module
            .add_requirement("definition", RequirementDraft::new("Definition"))
            .unwrap_err();
        assert!(matches!(err, AddNamedChildError::AlreadyExists(_)));
    }

    #[test]
    fn add_test_rejects_a_duplicate_name() {
        let mut module = ModuleDraft::default();
        module
            .add_test(
                "generic_test",
                TestDraft::new("Generic Test", ResultKindV1::FreeForm),
            )
            .unwrap();
        let err = module
            .add_test(
                "generic_test",
                TestDraft::new("Generic Test", ResultKindV1::FreeForm),
            )
            .unwrap_err();
        assert!(matches!(err, AddNamedChildError::AlreadyExists(_)));
    }

    #[test]
    fn add_result_rejects_a_duplicate_name() {
        let mut module = ModuleDraft::default();
        module
            .add_result("definition", minimal_result_draft())
            .unwrap();
        let err = module
            .add_result("definition", minimal_result_draft())
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
