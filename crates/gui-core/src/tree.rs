//! Read-side helpers: walking a module path, building the simplified
//! `TreeSnapshot`, and the three read-only `Command`s' logic. See
//! README's "`TreeSnapshot` is a simplified read model" section.

use disk::EntryName;
use logical::LogicalPath;
use logical::draft::{ModuleDraft, ProjectDraft};

use crate::{EntryDetail, EntryKind, EntryStatus, ModulePools, Outcome, ProjectState, TreeNode, TreeSnapshot};

fn draft_ref(state: &ProjectState) -> &ProjectDraft {
    match state {
        ProjectState::Draft(draft) => draft,
        ProjectState::Validated(validated) => validated.draft(),
    }
}

pub(crate) fn resolve_module<'a>(root: &'a ModuleDraft, path: &[EntryName]) -> Option<&'a ModuleDraft> {
    let mut current = root;
    for name in path {
        current = current.modules.get(name)?;
    }
    Some(current)
}

pub(crate) fn resolve_module_mut<'a>(
    root: &'a mut ModuleDraft,
    path: &[EntryName],
) -> Option<&'a mut ModuleDraft> {
    let mut current = root;
    for name in path {
        current = current.modules.get_mut(name)?;
    }
    Some(current)
}

pub(crate) fn build_tree_snapshot(state: &ProjectState, can_undo: bool, can_redo: bool) -> TreeSnapshot {
    let draft = draft_ref(state);
    let validated = match state {
        ProjectState::Validated(validated) => Some(validated),
        ProjectState::Draft(_) => None,
    };
    TreeSnapshot {
        root: build_tree_node(
            EntryName(draft.definition.name.clone()),
            EntryKind::Module,
            &draft.tree,
            validated,
            &[],
        ),
        can_undo,
        can_redo,
    }
}

fn build_tree_node(
    name: EntryName,
    kind: EntryKind,
    module: &ModuleDraft,
    validated: Option<&logical::ValidatedProject>,
    path_modules: &[EntryName],
) -> TreeNode {
    let mut children = Vec::new();

    for (child_name, child_module) in &module.modules {
        let mut child_path = path_modules.to_vec();
        child_path.push(child_name.clone());
        children.push(build_tree_node(
            child_name.clone(),
            EntryKind::Module,
            child_module,
            validated,
            &child_path,
        ));
    }

    for req_name in module.requirements.keys() {
        let status = match validated {
            None => EntryStatus::Unvalidated,
            Some(validated) => {
                let target = LogicalPath {
                    modules: path_modules.to_vec(),
                    name: req_name.clone(),
                };
                if validated.is_requirement_met(&target) {
                    EntryStatus::Met
                } else {
                    EntryStatus::Unmet
                }
            }
        };
        children.push(TreeNode {
            name: req_name.clone(),
            kind: EntryKind::Requirement,
            status,
            children: Vec::new(),
        });
    }

    // Tests/results don't have a "met" concept of their own — see
    // README's "Requirement-met semantics" (in `logical`'s README):
    // "met" is a property of a requirement, computed from its tests and
    // their results, not a status a test or result carries itself.
    for test_name in module.tests.keys() {
        children.push(TreeNode {
            name: test_name.clone(),
            kind: EntryKind::Test,
            status: EntryStatus::Unvalidated,
            children: Vec::new(),
        });
    }
    for result_name in module.results.keys() {
        children.push(TreeNode {
            name: result_name.clone(),
            kind: EntryKind::Result,
            status: EntryStatus::Unvalidated,
            children: Vec::new(),
        });
    }

    TreeNode {
        name,
        kind,
        status: EntryStatus::Unvalidated,
        children,
    }
}

pub(crate) fn get_entry_detail(state: &ProjectState, target: &LogicalPath, kind: EntryKind) -> Outcome {
    let draft = draft_ref(state);
    let Some(module) = resolve_module(&draft.tree, &target.modules) else {
        return Outcome::EntryDetail(None);
    };

    // Resolve against the pool matching `kind` only — a requirement,
    // test, and result can share a name within the same module, so
    // trying each pool in turn and returning the first hit (the previous
    // behavior) silently returned the wrong entry whenever that happened.
    let detail = match kind {
        EntryKind::Requirement => module.requirements.get(&target.name).map(|requirement| EntryDetail::Requirement {
            title: requirement.title.clone(),
            requirement_text: requirement.requirement_text.clone(),
            requirement_guidance: requirement.requirement_guidance.clone(),
            test_guidance: requirement.test_guidance.clone(),
            dependencies: requirement.dependencies.clone(),
            attachments: requirement.attachments.iter().cloned().collect(),
        }),
        EntryKind::Test => module.tests.get(&target.name).map(|test| EntryDetail::Test {
            title: test.title.clone(),
            result_kind: test.result_kind.clone(),
            attachments: test.attachments.iter().cloned().collect(),
            template_files: test.template.iter().cloned().collect(),
        }),
        EntryKind::Result => module.results.get(&target.name).map(|result| EntryDetail::Result {
            title: result.title.clone(),
            requirement_path: result.requirement_path.0.clone(),
            requirement_commit: result.requirement_commit.clone(),
            test_path: result.test_path.0.clone(),
            test_commit: result.test_commit.clone(),
            attachments: result.attachments.iter().cloned().collect(),
        }),
        EntryKind::Module => None,
    };
    Outcome::EntryDetail(detail)
}

pub(crate) fn is_requirement_met(state: &ProjectState, target: &LogicalPath) -> Outcome {
    let met = match state {
        // Unvalidated: "met" isn't meaningfully answerable (no resolved
        // references to check), so this reports `false` rather than
        // erroring — same "not an error, just not there yet" spirit as
        // `logical`'s own "historical results are not errors" note.
        ProjectState::Draft(_) => false,
        ProjectState::Validated(validated) => validated.is_requirement_met(target),
    };
    Outcome::RequirementMet(met)
}

pub(crate) fn dependency_chain(state: &ProjectState, target: &LogicalPath) -> Outcome {
    let chain = match state {
        ProjectState::Draft(_) => Vec::new(),
        ProjectState::Validated(validated) => validated.dependency_chain(target),
    };
    Outcome::DependencyChain(chain)
}

pub(crate) fn get_module_pools(state: &ProjectState, module: &[EntryName]) -> Outcome {
    let draft = draft_ref(state);
    let Some(module) = resolve_module(&draft.tree, module) else {
        return Outcome::ModulePools(None);
    };
    Outcome::ModulePools(Some(ModulePools {
        attachments: module.attachments.iter().cloned().collect(),
        templates: module.templates.iter().cloned().collect(),
    }))
}
