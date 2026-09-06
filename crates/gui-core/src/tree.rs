//! Read-side helpers: walking a module path, building the simplified
//! `TreeSnapshot`, and the three read-only `Command`s' logic. See
//! README's "`TreeSnapshot` is a simplified read model" section.

use disk::EntryName;
use logical::LogicalPath;
use logical::draft::{ModuleDraft, ProjectDraft};

use crate::{
    EntryDetail, EntryKind, EntryPath, EntryStatus, ModulePools, ModuleSummary, Outcome, ProjectState,
    RequirementMetStatus, StatusV1, TreeNode, TreeSnapshot,
};

fn draft_ref(state: &ProjectState) -> &ProjectDraft {
    match state {
        ProjectState::Draft(draft) => draft,
        ProjectState::Validated(validated) => validated.draft(),
    }
}

/// `Command::FindReferences`'s read handler — see that command's own doc
/// comment.
pub(crate) fn find_references(state: &ProjectState, target: &logical::ReferenceTarget) -> Outcome {
    Outcome::FindReferences(logical::find_references(draft_ref(state), target))
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

    for (req_name, requirement) in &module.requirements {
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
        // A result's place in the tree follows its place on disk now —
        // nested under its owning requirement — rather than flat
        // alongside it. Results don't have a "met" concept of their own
        // any more than tests do (see README's "Requirement-met
        // semantics" in `logical`'s README): that's a property of the
        // requirement, computed from its tests and their results.
        let result_children = requirement
            .results
            .keys()
            .map(|result_name| TreeNode {
                name: result_name.clone(),
                kind: EntryKind::Result,
                status: EntryStatus::Unvalidated,
                children: Vec::new(),
            })
            .collect();
        children.push(TreeNode {
            name: req_name.clone(),
            kind: EntryKind::Requirement,
            status,
            children: result_children,
        });
    }

    for test_name in module.tests.keys() {
        children.push(TreeNode {
            name: test_name.clone(),
            kind: EntryKind::Test,
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

pub(crate) fn get_entry_detail(state: &ProjectState, target: &EntryPath) -> Outcome {
    let draft = draft_ref(state);

    let detail = match target {
        EntryPath::Requirement(target) => resolve_module(&draft.tree, &target.modules)
            .and_then(|module| module.requirements.get(&target.name))
            .map(|requirement| EntryDetail::Requirement {
                title: requirement.title.clone(),
                requirement_text: requirement.requirement_text.clone(),
                requirement_guidance: requirement.requirement_guidance.clone(),
                test_guidance: requirement.test_guidance.clone(),
                dependencies: requirement.dependencies.clone(),
                attachments: requirement.attachments.iter().cloned().collect(),
                met_status: requirement_met_status(state, target),
                results: logical::results_for_requirement(&draft.tree, target),
                original: Box::new(requirement.clone()),
            }),
        EntryPath::Test(target) => resolve_module(&draft.tree, &target.modules)
            .and_then(|module| module.tests.get(&target.name))
            .map(|test| EntryDetail::Test {
                title: test.title.clone(),
                test_text: test.test_text.clone(),
                result_kind: test.result_kind.clone(),
                attachments: test.attachments.iter().cloned().collect(),
                template_files: test.template.iter().cloned().collect(),
                original: Box::new(test.clone()),
            }),
        EntryPath::Result(target) => resolve_module(&draft.tree, &target.requirement.modules)
            .and_then(|module| module.requirements.get(&target.requirement.name))
            .and_then(|requirement| requirement.results.get(&target.name))
            .map(|result| EntryDetail::Result {
                title: result.title.clone(),
                requirement: target.requirement.clone(),
                requirement_commit: result.requirement_commit.clone(),
                test_path: result.test_path.0.clone(),
                test_commit: result.test_commit.clone(),
                attachments: result.attachments.iter().cloned().collect(),
                original: Box::new(result.clone()),
            }),
    };
    Outcome::EntryDetail(detail)
}

pub(crate) fn get_requirement_met_status(state: &ProjectState, target: &LogicalPath) -> Outcome {
    Outcome::RequirementMetStatus(requirement_met_status(state, target))
}

/// The one place `Draft`/`Validated` gets turned into a `RequirementMetStatus`
/// — shared by `get_entry_detail`'s own `met_status` field and
/// `get_requirement_met_status`, so there's exactly one answer to "is this
/// requirement met, and if not, why" rather than two copies that could
/// drift apart.
fn requirement_met_status(state: &ProjectState, target: &LogicalPath) -> RequirementMetStatus {
    match state {
        // Unvalidated: Met/Unmet isn't meaningfully answerable (no
        // resolved references to check) — same "not an error, just not
        // there yet" spirit as `logical`'s own "historical results are
        // not errors" note.
        ProjectState::Draft(_) => RequirementMetStatus::Unvalidated,
        ProjectState::Validated(validated) => match validated.requirement_unmet_reason(target) {
            None => RequirementMetStatus::Met,
            Some(reason) => RequirementMetStatus::Unmet(reason),
        },
    }
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

pub(crate) fn get_module_summary(state: &ProjectState, module: &[EntryName]) -> Outcome {
    let draft = draft_ref(state);
    let Some(root) = resolve_module(&draft.tree, module) else {
        return Outcome::ModuleSummary(None);
    };
    let mut summary = ModuleSummary {
        validated: matches!(state, ProjectState::Validated(_)),
        ..ModuleSummary::default()
    };
    accumulate_module_summary(state, root, module, &mut summary);
    Outcome::ModuleSummary(Some(summary))
}

/// Recursion shape mirrors `build_tree_node`'s own walk, but accumulates
/// counts into `summary` instead of building `TreeNode`s.
fn accumulate_module_summary(
    state: &ProjectState,
    module: &ModuleDraft,
    path_modules: &[EntryName],
    summary: &mut ModuleSummary,
) {
    summary.submodule_count += module.modules.len();
    summary.requirement_count += module.requirements.len();
    summary.test_count += module.tests.len();
    summary.result_count += module.requirements.values().map(|r| r.results.len()).sum::<usize>();

    for (req_name, requirement) in &module.requirements {
        let target = LogicalPath {
            modules: path_modules.to_vec(),
            name: req_name.clone(),
        };
        match requirement_met_status(state, &target) {
            RequirementMetStatus::Met => summary.requirements_met += 1,
            RequirementMetStatus::Unmet(_) => summary.requirements_unmet += 1,
            RequirementMetStatus::Unvalidated => {}
        }
        for result in requirement.results.values() {
            match result.status {
                StatusV1::Pass => summary.results_pass += 1,
                StatusV1::Fail => summary.results_fail += 1,
                StatusV1::Incomplete => summary.results_incomplete += 1,
            }
        }
    }

    for (child_name, child_module) in &module.modules {
        let mut child_path = path_modules.to_vec();
        child_path.push(child_name.clone());
        accumulate_module_summary(state, child_module, &child_path, summary);
    }
}
