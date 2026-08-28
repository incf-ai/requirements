use disk::EntryName;

use crate::LogicalPath;
use crate::draft::{ModuleDraft, RequirementDraft, TestDraft};

/// Walks `path` (a chain of submodule names from the project root) down
/// through nested `ModuleDraft.modules` maps. Shared by validation
/// (resolving `Module*ReferenceV1`) and `ValidatedProject`'s queries.
pub(crate) fn get_module<'a>(root: &'a ModuleDraft, path: &[EntryName]) -> Option<&'a ModuleDraft> {
    let mut current = root;
    for name in path {
        current = current.modules.get(name)?;
    }
    Some(current)
}

pub(crate) fn get_requirement<'a>(
    root: &'a ModuleDraft,
    target: &LogicalPath,
) -> Option<&'a RequirementDraft> {
    get_module(root, &target.modules)?
        .requirements
        .get(&target.name)
}

pub(crate) fn get_test<'a>(root: &'a ModuleDraft, target: &LogicalPath) -> Option<&'a TestDraft> {
    get_module(root, &target.modules)?.tests.get(&target.name)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::draft::{RequirementDraft as Req, TestDraft as Test};
    use disk::ResultKindV1;

    fn sample_tree() -> ModuleDraft {
        let mut root = ModuleDraft::default();
        root.add_requirement("definition", Req::new("Definition"))
            .unwrap();
        root.add_test(
            "generic_test",
            Test::new("Generic Test", ResultKindV1::FreeForm),
        )
        .unwrap();
        root.add_module("embeddings").unwrap();
        root.modules
            .get_mut(&EntryName("embeddings".to_string()))
            .unwrap()
            .add_requirement("nested", Req::new("Nested"))
            .unwrap();
        root
    }

    #[test]
    fn finds_the_project_root_module_with_an_empty_path() {
        let root = sample_tree();
        assert!(get_module(&root, &[]).is_some());
    }

    #[test]
    fn finds_a_nested_module() {
        let root = sample_tree();
        assert!(get_module(&root, &[EntryName("embeddings".to_string())]).is_some());
    }

    #[test]
    fn returns_none_for_a_missing_module() {
        let root = sample_tree();
        assert!(get_module(&root, &[EntryName("nonexistent".to_string())]).is_none());
    }

    #[test]
    fn finds_a_root_requirement() {
        let root = sample_tree();
        let target = LogicalPath::root(EntryName("definition".to_string()));
        assert!(get_requirement(&root, &target).is_some());
    }

    #[test]
    fn finds_a_nested_requirement() {
        let root = sample_tree();
        let target = LogicalPath {
            modules: vec![EntryName("embeddings".to_string())],
            name: EntryName("nested".to_string()),
        };
        assert!(get_requirement(&root, &target).is_some());
    }

    #[test]
    fn returns_none_for_a_requirement_in_a_missing_module() {
        let root = sample_tree();
        let target = LogicalPath {
            modules: vec![EntryName("nonexistent".to_string())],
            name: EntryName("definition".to_string()),
        };
        assert!(get_requirement(&root, &target).is_none());
    }

    #[test]
    fn finds_a_root_test() {
        let root = sample_tree();
        let target = LogicalPath::root(EntryName("generic_test".to_string()));
        assert!(get_test(&root, &target).is_some());
    }

    #[test]
    fn returns_none_for_a_missing_test() {
        let root = sample_tree();
        let target = LogicalPath::root(EntryName("nonexistent".to_string()));
        assert!(get_test(&root, &target).is_none());
    }

    #[test]
    fn returns_none_for_a_test_in_a_missing_module() {
        let root = sample_tree();
        let target = LogicalPath {
            modules: vec![EntryName("nonexistent".to_string())],
            name: EntryName("generic_test".to_string()),
        };
        assert!(get_test(&root, &target).is_none());
    }
}
