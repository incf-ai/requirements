use std::collections::BTreeMap;

use disk::{
    AttachmentFile, ModuleTree, ProjectOnDisk, RequirementOnDisk, ResultOnDisk, SubmoduleOnDisk,
    TestOnDisk,
};

use crate::draft::{ModuleDraft, ProjectDraft, RequirementDraft, ResultDraft, TestDraft};

/// Reshapes a loaded `disk::ProjectOnDisk` into a `ProjectDraft` — see
/// `crates/logical/README.md`'s "Converting to/from `disk`": infallible,
/// `Vec` becomes `BTreeMap`/`BTreeSet`, nothing is dropped or resolved.
pub fn import_project(on_disk: ProjectOnDisk) -> ProjectDraft {
    ProjectDraft {
        definition: on_disk.definition,
        tree: import_module_tree(on_disk.tree),
    }
}

fn attachment_paths(files: Vec<AttachmentFile>) -> std::collections::BTreeSet<std::path::PathBuf> {
    files.into_iter().map(|file| file.path).collect()
}

fn import_module_tree(tree: ModuleTree) -> ModuleDraft {
    ModuleDraft {
        attachments: attachment_paths(tree.attachments),
        templates: attachment_paths(tree.templates),
        requirements: named_map(tree.requirements, |r| {
            (r.name.clone(), import_requirement(r))
        }),
        tests: named_map(tree.tests, |t| (t.name.clone(), import_test(t))),
        results: named_map(tree.results, |r| (r.name.clone(), import_result(r))),
        modules: named_map(tree.modules, |m| (m.name.clone(), import_submodule(m))),
    }
}

fn named_map<T, U>(
    items: Vec<T>,
    f: impl Fn(T) -> (disk::EntryName, U),
) -> BTreeMap<disk::EntryName, U> {
    items.into_iter().map(f).collect()
}

fn import_submodule(submodule: SubmoduleOnDisk) -> ModuleDraft {
    import_module_tree(submodule.tree)
}

fn import_requirement(requirement: RequirementOnDisk) -> RequirementDraft {
    let definition = requirement.definition;

    let mut tests = Vec::new();
    tests.extend(definition.test);
    if let Some(more) = definition.tests {
        tests.extend(more.iter().cloned());
    }

    let mut dependencies = Vec::new();
    dependencies.extend(definition.dependency);
    if let Some(more) = definition.dependencies {
        dependencies.extend(more.iter().cloned());
    }

    let mut attachment_refs = Vec::new();
    attachment_refs.extend(definition.attachment);
    if let Some(more) = definition.attachments {
        attachment_refs.extend(more.iter().cloned());
    }

    RequirementDraft {
        title: definition.title,
        requirement_text: requirement.requirement_text,
        requirement_guidance: requirement.requirement_guidance,
        test_guidance: requirement.test_guidance,
        tests,
        dependencies,
        attachments: attachment_paths(requirement.attachments),
        attachment_refs,
        include_attachments_in_commit: definition.include_attachments_in_commit,
        commit: Some(requirement.commit),
    }
}

fn import_test(test: TestOnDisk) -> TestDraft {
    let definition = test.definition;

    let mut attachment_refs = Vec::new();
    attachment_refs.extend(definition.attachment);
    if let Some(more) = definition.attachments {
        attachment_refs.extend(more.iter().cloned());
    }

    let mut template_refs = Vec::new();
    template_refs.extend(definition.template);
    if let Some(more) = definition.templates {
        template_refs.extend(more.iter().cloned());
    }

    TestDraft {
        title: definition.title,
        result_kind: definition.result_kind,
        attachments: attachment_paths(test.attachments),
        attachment_refs,
        template: attachment_paths(test.template),
        template_refs,
        include_attachments_in_commit: definition.include_attachments_in_commit,
        include_template_in_commit: definition.include_template_in_commit,
        commit: Some(test.commit),
    }
}

fn import_result(result: ResultOnDisk) -> ResultDraft {
    let definition = result.definition;

    let mut attachment_refs = Vec::new();
    attachment_refs.extend(definition.attachment);
    if let Some(more) = definition.attachments {
        attachment_refs.extend(more.iter().cloned());
    }

    ResultDraft {
        title: definition.title,
        requirement_path: definition.requirement_path,
        requirement_commit: definition.requirement_commit,
        test_path: definition.test_path,
        test_commit: definition.test_commit,
        status: definition.status,
        attachments: attachment_paths(result.attachments),
        attachment_refs,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn imports_a_requirement_with_plural_tests_dependencies_and_attachments() {
        let requirement = disk::RequirementOnDisk {
            name: disk::EntryName("definition".to_string()),
            definition: disk::RequirementDefinitionV1 {
                title: "Definition".to_string(),
                test: None,
                tests: Some(
                    nunny::Vec::new(vec![
                        disk::TestReferenceKind::TestReferenceV1(disk::LocalGitReference {
                            path: disk::ReferencePath("/tests/a".to_string()),
                            commit: "c1".to_string(),
                        }),
                        disk::TestReferenceKind::TestReferenceV1(disk::LocalGitReference {
                            path: disk::ReferencePath("/tests/b".to_string()),
                            commit: "c1".to_string(),
                        }),
                    ])
                    .unwrap(),
                ),
                dependency: None,
                dependencies: Some(
                    nunny::Vec::new(vec![
                        disk::DependencyReferenceKind::Submodules,
                        disk::DependencyReferenceKind::Submodules,
                    ])
                    .unwrap(),
                ),
                attachment: None,
                attachments: Some(
                    nunny::Vec::new(vec![
                        disk::AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                            name: disk::EntryName("a".to_string()),
                            path: std::path::PathBuf::from("a.txt"),
                        },
                        disk::AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                            name: disk::EntryName("b".to_string()),
                            path: std::path::PathBuf::from("b.txt"),
                        },
                    ])
                    .unwrap(),
                ),
                include_attachments_in_commit: true,
            },
            requirement_text: String::new(),
            requirement_guidance: None,
            test_guidance: None,
            attachments: Vec::new(),
            commit: "c1".to_string(),
        };

        let draft = import_requirement(requirement);
        assert_eq!(draft.tests.len(), 2);
        assert_eq!(draft.dependencies.len(), 2);
        assert_eq!(draft.attachment_refs.len(), 2);
    }

    #[test]
    fn imports_a_test_with_plural_attachments() {
        let test = disk::TestOnDisk {
            name: disk::EntryName("generic_test".to_string()),
            definition: disk::TestV1 {
                title: "Generic Test".to_string(),
                result_kind: disk::ResultKindV1::FreeForm,
                attachment: None,
                attachments: Some(
                    nunny::Vec::new(vec![
                        disk::AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                            name: disk::EntryName("a".to_string()),
                            path: std::path::PathBuf::from("a.txt"),
                        },
                        disk::AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                            name: disk::EntryName("b".to_string()),
                            path: std::path::PathBuf::from("b.txt"),
                        },
                    ])
                    .unwrap(),
                ),
                template: None,
                templates: None,
                include_attachments_in_commit: true,
                include_template_in_commit: true,
            },
            test_text: String::new(),
            attachments: Vec::new(),
            template: Vec::new(),
            commit: "c1".to_string(),
        };

        let draft = import_test(test);
        assert_eq!(draft.attachment_refs.len(), 2);
    }

    #[test]
    fn imports_the_whole_sample_project() {
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);

        assert_eq!(draft.definition.name, "Capstone");
        assert_eq!(draft.tree.requirements.len(), 5);
        assert_eq!(draft.tree.tests.len(), 5);
        assert_eq!(draft.tree.results.len(), 5);
        assert_eq!(draft.tree.modules.len(), 5);
    }

    #[test]
    fn imports_a_requirement_with_a_test_and_dependency_reference() {
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);

        let definition = draft
            .tree
            .requirements
            .get(&disk::EntryName("definition".to_string()))
            .unwrap();
        assert_eq!(definition.title, "Definition");
        assert_eq!(definition.tests.len(), 1);
        assert_eq!(definition.dependencies.len(), 1);
    }

    #[test]
    fn imports_local_and_module_attachment_references() {
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);

        let discovery = draft
            .tree
            .requirements
            .get(&disk::EntryName("discovery".to_string()))
            .unwrap();
        assert_eq!(discovery.attachment_refs.len(), 1);
        assert!(matches!(
            discovery.attachment_refs[0],
            disk::AttachmentReferenceKind::LocalAttachmentReferenceV1 { .. }
        ));

        assert!(!draft.tree.attachments.is_empty());
    }

    #[test]
    fn imports_a_test_with_local_and_module_template_references() {
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);

        let review = draft
            .tree
            .tests
            .get(&disk::EntryName("example_review".to_string()))
            .unwrap();
        assert_eq!(review.template_refs.len(), 2);
        assert!(!draft.tree.templates.is_empty());
    }

    #[test]
    fn imports_a_result_with_requirement_and_test_commit_pairs() {
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);

        let definition = draft
            .tree
            .results
            .get(&disk::EntryName("definition".to_string()))
            .unwrap();
        assert_eq!(definition.requirement_path.0, "requirements/definition");
        assert_eq!(definition.test_path.0, "/tests/generic_inspection");
    }

    #[test]
    fn imports_an_empty_submodule() {
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);

        assert!(
            draft
                .tree
                .modules
                .contains_key(&disk::EntryName("setup".to_string()))
        );
    }
}
