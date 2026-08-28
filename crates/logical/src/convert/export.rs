use std::collections::BTreeSet;
use std::path::PathBuf;

use disk::{
    AttachmentFile, EntryName, ModuleTree, ProjectOnDisk, RequirementDefinitionV1,
    RequirementOnDisk, ResultOnDisk, ResultsV1, SubmoduleOnDisk, SubmoduleV1, TestOnDisk, TestV1,
};

use crate::draft::{ModuleDraft, ProjectDraft, RequirementDraft, ResultDraft, TestDraft};

/// Reshapes a `ProjectDraft` back into `disk`'s `Vec`/`*OnDisk` shapes —
/// see `crates/logical/README.md`'s "Converting to/from `disk`" and
/// "Decisions made" #6: the disk-level `test`/`tests` (etc.) mutual
/// exclusion rule is enforced *by construction* here — `split_singular_plural`
/// only ever sets one of the pair, never both, so there's no code path
/// that could produce an invalid `disk::RequirementDefinitionV1`.
///
/// `AttachmentFile::commit` and `RequirementOnDisk`/`TestOnDisk`::commit are
/// filled with a placeholder: `disk`'s save path never reads them (commit
/// is a load-time-only derived field — see `disk`'s README), so nothing
/// downstream depends on the value written here.
pub fn export_project(draft: &ProjectDraft) -> ProjectOnDisk {
    ProjectOnDisk {
        definition: draft.definition.clone(),
        tree: export_module_tree(&draft.tree),
    }
}

fn export_module_tree(module: &ModuleDraft) -> ModuleTree {
    ModuleTree {
        attachments: export_pool(&module.attachments),
        templates: export_pool(&module.templates),
        requirements: module
            .requirements
            .iter()
            .map(|(name, requirement)| export_requirement(name, requirement))
            .collect(),
        tests: module
            .tests
            .iter()
            .map(|(name, test)| export_test(name, test))
            .collect(),
        results: module
            .results
            .iter()
            .map(|(name, result)| export_result(name, result))
            .collect(),
        modules: module
            .modules
            .iter()
            .map(|(name, submodule)| export_submodule(name, submodule))
            .collect(),
    }
}

fn export_pool(pool: &BTreeSet<PathBuf>) -> Vec<AttachmentFile> {
    pool.iter()
        .cloned()
        .map(|path| AttachmentFile {
            path,
            commit: String::new(),
        })
        .collect()
}

/// Splits a collapsed `Vec<T>` back into `disk`'s `Option<T>`/
/// `Option<NonEmptyVec<T>>` pair — never both `Some`.
fn split_singular_plural<T>(mut items: Vec<T>) -> (Option<T>, Option<nunny::Vec<T>>) {
    match items.len() {
        0 => (None, None),
        1 => (items.pop(), None),
        _ => (
            None,
            Some(
                nunny::Vec::new(items).unwrap_or_else(|_| unreachable!("length > 1 checked above")),
            ),
        ),
    }
}

fn export_requirement(name: &EntryName, requirement: &RequirementDraft) -> RequirementOnDisk {
    let (test, tests) = split_singular_plural(requirement.tests.clone());
    let (dependency, dependencies) = split_singular_plural(requirement.dependencies.clone());
    let (attachment, attachments) = split_singular_plural(requirement.attachment_refs.clone());

    RequirementOnDisk {
        name: name.clone(),
        definition: RequirementDefinitionV1 {
            title: requirement.title.clone(),
            test,
            tests,
            dependency,
            dependencies,
            attachment,
            attachments,
            include_attachments_in_commit: requirement.include_attachments_in_commit,
        },
        requirement_text: requirement.requirement_text.clone(),
        requirement_guidance: requirement.requirement_guidance.clone(),
        test_guidance: requirement.test_guidance.clone(),
        attachments: export_pool(&requirement.attachments),
        commit: requirement.commit.clone().unwrap_or_default(),
    }
}

fn export_test(name: &EntryName, test: &TestDraft) -> TestOnDisk {
    let (attachment, attachments) = split_singular_plural(test.attachment_refs.clone());
    let (template, templates) = split_singular_plural(test.template_refs.clone());

    TestOnDisk {
        name: name.clone(),
        definition: TestV1 {
            title: test.title.clone(),
            result_kind: test.result_kind.clone(),
            attachment,
            attachments,
            template,
            templates,
            include_attachments_in_commit: test.include_attachments_in_commit,
            include_template_in_commit: test.include_template_in_commit,
        },
        test_text: String::new(),
        attachments: export_pool(&test.attachments),
        template: export_pool(&test.template),
        commit: test.commit.clone().unwrap_or_default(),
    }
}

fn export_result(name: &EntryName, result: &ResultDraft) -> ResultOnDisk {
    let (attachment, attachments) = split_singular_plural(result.attachment_refs.clone());

    ResultOnDisk {
        name: name.clone(),
        definition: ResultsV1 {
            title: result.title.clone(),
            requirement_path: result.requirement_path.clone(),
            requirement_commit: result.requirement_commit.clone(),
            test_path: result.test_path.clone(),
            test_commit: result.test_commit.clone(),
            status: result.status.clone(),
            attachment,
            attachments,
        },
        attachments: export_pool(&result.attachments),
    }
}

fn export_submodule(name: &EntryName, module: &ModuleDraft) -> SubmoduleOnDisk {
    SubmoduleOnDisk {
        name: name.clone(),
        definition: SubmoduleV1 {
            name: name.to_string(),
        },
        tree: export_module_tree(module),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::convert::import::import_project;
    use crate::test_support::FixedGit;
    use syscalls::StdFilesystem;

    fn sample_project_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    #[test]
    fn export_then_import_round_trips_counts() {
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);

        let exported = export_project(&draft);
        let reimported = import_project(exported);

        assert_eq!(reimported.definition.name, draft.definition.name);
        assert_eq!(
            reimported.tree.requirements.len(),
            draft.tree.requirements.len()
        );
        assert_eq!(reimported.tree.tests.len(), draft.tree.tests.len());
        assert_eq!(reimported.tree.results.len(), draft.tree.results.len());
        assert_eq!(reimported.tree.modules.len(), draft.tree.modules.len());
    }

    #[test]
    fn export_never_sets_both_singular_and_plural_fields() {
        // `split_singular_plural_handles_zero_one_and_many` below already
        // proves the underlying function never produces both Some — a
        // structural guarantee that (correctly) can never be violated by
        // any real data, sample project included. This just confirms the
        // plural field lands correctly for an entity that actually has
        // more than one reference: `example_review`'s two `templates`.
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &sample_project_dir()).unwrap();
        let draft = import_project(on_disk);
        let exported = export_project(&draft);

        let review = exported
            .tree
            .tests
            .iter()
            .find(|test| test.name.as_str() == "example_review")
            .expect("sample project has an example_review test");
        assert!(review.definition.template.is_none());
        assert_eq!(
            review.definition.templates.as_ref().map(|t| t.len()),
            Some(2)
        );
    }

    #[test]
    fn split_singular_plural_handles_zero_one_and_many() {
        assert_eq!(split_singular_plural::<u8>(vec![]), (None, None));
        assert_eq!(split_singular_plural(vec![1u8]), (Some(1), None));
        let (single, many) = split_singular_plural(vec![1u8, 2, 3]);
        assert_eq!(single, None);
        assert_eq!(many.unwrap().len(), 3);
    }
}
