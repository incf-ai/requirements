use disk::RootV1;

use crate::draft::module::ModuleDraft;

/// A whole project, freely mutable. See `crates/logical/README.md`'s
/// "Draft vs. validated" section: nothing about cross-entity consistency
/// is enforced here — only `validate()` (producing a `ValidatedProject`)
/// checks that.
#[derive(Debug, Clone)]
pub struct ProjectDraft {
    pub definition: RootV1,
    pub tree: ModuleDraft,
}

pub fn create_project(name: impl Into<String>) -> ProjectDraft {
    ProjectDraft {
        definition: RootV1 { name: name.into() },
        tree: ModuleDraft::default(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn create_project_starts_with_an_empty_tree() {
        let project = create_project("Capstone");
        assert_eq!(project.definition.name, "Capstone");
        assert!(project.tree.requirements.is_empty());
        assert!(project.tree.tests.is_empty());
        assert!(project.tree.results.is_empty());
        assert!(project.tree.modules.is_empty());
        assert!(project.tree.attachments.is_empty());
        assert!(project.tree.templates.is_empty());
    }
}
