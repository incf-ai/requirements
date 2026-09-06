use std::cmp::Reverse;

use disk::{DependencyReferenceKind, EntryName, LocalGitReference, ReferencePath, TestReferenceKind};
use thiserror::Error;

use crate::LogicalPath;
use crate::draft::ProjectDraft;
use crate::lookup::{get_module, get_module_mut, get_requirement, get_result, get_result_mut, get_test};
use crate::path::{ResultPath, format_reference_path, parse_reference_path};

/// What a rename/recreate is changing the identity of — both the "old"
/// value `find_references` searches for, and (when repairing) the "new"
/// value references should be rewritten to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceTarget {
    Requirement(LogicalPath),
    Test(LogicalPath),
    /// The full path (from the project root) of a module. Matches not
    /// just direct references to the module itself, but any reference
    /// whose target lives inside it — per this feature's "include
    /// descendants" scope decision for module rename.
    Module(Vec<EntryName>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntityKind {
    Requirement,
    Test,
}

/// One reference that a rename of some `ReferenceTarget` will break, and
/// exactly which field on the referrer holds it — enough to repair or
/// remove that one reference without touching any of its siblings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceSite {
    pub referrer: LogicalPath,
    pub kind: ReferenceSiteKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceSiteKind {
    /// `RequirementDraft.tests[index]`.
    RequirementTestReference { index: usize },
    /// `RequirementDraft.dependencies[index]` — always a
    /// `RequirementReferenceV1` entry; `RemoteReferenceV1`/`Submodules`
    /// dependencies can't name an in-project entity, so they're never
    /// recorded here.
    RequirementDependency { index: usize },
    /// `ResultDraft.test_path`/`test_commit`, on the result named
    /// `result_name` nested under the referrer requirement. A result's
    /// *requirement* is no longer a field that can go stale — it's
    /// structural (the result lives inside `RequirementDraft.results`), so
    /// only its `test_path` reference can still break on a rename. A
    /// required field, not a list entry: there's no empty state to fall
    /// back to, so `ReferenceAction::Remove` on this site deletes the
    /// whole result.
    ResultTestRef { result_name: EntryName },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceAction {
    /// Rewrite the reference to point at the new target (requires one).
    Repair,
    /// Delete the reference (or, for a result's required fields, the
    /// whole `ResultDraft`).
    Remove,
    /// Leave the reference exactly as it is.
    Ignore,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReferenceRepairError {
    #[error("no referrer at `{0}` in the project")]
    UnknownReferrer(LogicalPath),
    #[error("referrer at `{referrer}` has no entry at index {index} for this reference kind")]
    MissingIndex { referrer: LogicalPath, index: usize },
    #[error("referrer at `{referrer}` has a dependency at index {index} that isn't a local requirement reference")]
    NotALocalRequirementReference { referrer: LogicalPath, index: usize },
    #[error("`Repair` was requested but no new target was given")]
    RepairWithoutNewTarget,
    #[error(
        "old and new targets don't match (must be the same kind), or a module rename's new prefix couldn't be applied to the resolved path"
    )]
    TargetMismatch,
}

/// True if `resolved`'s literal, on-disk `modules` chain includes
/// `old_module_path` — i.e. the module being renamed sits somewhere along
/// the string this reference actually spells out, so renaming it breaks
/// this reference. A *relative* reference only spells out the modules it
/// dips into beyond `current_module` (that prefix is implicit, re-derived
/// fresh from the live tree every time `parse_reference_path` runs); an
/// ancestor of `current_module` changing name doesn't touch the stored
/// string at all, so it never breaks a relative reference — only a dip
/// that names the renamed module explicitly does.
fn module_rename_breaks(
    resolved_modules: &[EntryName],
    is_absolute: bool,
    current_module: &[EntryName],
    old_module_path: &[EntryName],
) -> bool {
    if is_absolute {
        resolved_modules.starts_with(old_module_path)
    } else {
        old_module_path.len() > current_module.len()
            && resolved_modules.len() >= old_module_path.len()
            && &resolved_modules[..old_module_path.len()] == old_module_path
    }
}

fn matches_target(
    resolved: &LogicalPath,
    raw: &ReferencePath,
    current_module: &[EntryName],
    resolved_kind: EntityKind,
    target: &ReferenceTarget,
) -> bool {
    match target {
        ReferenceTarget::Requirement(path) => {
            resolved_kind == EntityKind::Requirement && resolved == path
        }
        ReferenceTarget::Test(path) => resolved_kind == EntityKind::Test && resolved == path,
        ReferenceTarget::Module(old_module_path) => module_rename_breaks(
            &resolved.modules,
            raw.0.starts_with('/'),
            current_module,
            old_module_path,
        ),
    }
}

/// Every place in `project` that references `target`, or (for a module
/// target) anything living inside it — the set of references a rename of
/// `target` would break.
pub fn find_references(project: &ProjectDraft, target: &ReferenceTarget) -> Vec<ReferenceSite> {
    let mut sites = Vec::new();
    walk_module(project, &mut Vec::new(), target, &mut sites);
    sites
}

fn walk_module(
    project: &ProjectDraft,
    current_module: &mut Vec<EntryName>,
    target: &ReferenceTarget,
    sites: &mut Vec<ReferenceSite>,
) {
    // `current_module` is always a path this same function just descended
    // into via `module.modules.keys()`, so it always resolves.
    let module = get_module(&project.tree, current_module)
        .expect("current_module is always a live path into the tree");

    for (name, requirement) in &module.requirements {
        let referrer = LogicalPath {
            modules: current_module.clone(),
            name: name.clone(),
        };
        for (index, test_ref) in requirement.tests.iter().enumerate() {
            let TestReferenceKind::TestReferenceV1(local) = test_ref;
            if let Ok(resolved) = parse_reference_path(&local.path, current_module, "tests")
                && matches_target(&resolved, &local.path, current_module, EntityKind::Test, target)
            {
                sites.push(ReferenceSite {
                    referrer: referrer.clone(),
                    kind: ReferenceSiteKind::RequirementTestReference { index },
                });
            }
        }
        for (index, dependency) in requirement.dependencies.iter().enumerate() {
            let DependencyReferenceKind::RequirementReferenceV1(local) = dependency else {
                continue;
            };
            if let Ok(resolved) = parse_reference_path(&local.path, current_module, "requirements")
                && matches_target(
                    &resolved,
                    &local.path,
                    current_module,
                    EntityKind::Requirement,
                    target,
                )
            {
                sites.push(ReferenceSite {
                    referrer: referrer.clone(),
                    kind: ReferenceSiteKind::RequirementDependency { index },
                });
            }
        }
        for (result_name, result) in &requirement.results {
            if let Ok(resolved) = parse_reference_path(&result.test_path, current_module, "tests")
                && matches_target(&resolved, &result.test_path, current_module, EntityKind::Test, target)
            {
                sites.push(ReferenceSite {
                    referrer: referrer.clone(),
                    kind: ReferenceSiteKind::ResultTestRef {
                        result_name: result_name.clone(),
                    },
                });
            }
        }
    }

    let submodule_names: Vec<EntryName> = module.modules.keys().cloned().collect();
    for name in submodule_names {
        current_module.push(name);
        walk_module(project, current_module, target, sites);
        current_module.pop();
    }
}

/// Where a resolved reference should point after a rename — `None` when
/// `old_target`/`new_target` don't line up (mismatched kinds, or a module
/// rename whose new prefix can't apply to this particular resolved path).
fn repaired_resolved_path(
    old_target: &ReferenceTarget,
    new_target: &ReferenceTarget,
    old_resolved: &LogicalPath,
) -> Option<LogicalPath> {
    match (old_target, new_target) {
        (ReferenceTarget::Requirement(_), ReferenceTarget::Requirement(new_path)) => {
            Some(new_path.clone())
        }
        (ReferenceTarget::Test(_), ReferenceTarget::Test(new_path)) => Some(new_path.clone()),
        (ReferenceTarget::Module(old_prefix), ReferenceTarget::Module(new_prefix))
            if old_resolved.modules.starts_with(old_prefix.as_slice()) =>
        {
            let mut modules = new_prefix.clone();
            modules.extend_from_slice(&old_resolved.modules[old_prefix.len()..]);
            Some(LogicalPath {
                modules,
                name: old_resolved.name.clone(),
            })
        }
        _ => None,
    }
}

/// Computes the repaired `(ReferencePath, commit)` pair for a single
/// reference currently stored as `raw` (found on a referrer living at
/// `referrer_modules`), preserving `raw`'s absolute-vs-relative style and
/// refreshing the pinned commit from the retargeted entity's current one
/// (empty string if it has none yet — same "not yet saved" convention
/// used when a reference is first authored).
fn compute_repair(
    project: &ProjectDraft,
    referrer_modules: &[EntryName],
    raw: &ReferencePath,
    kind: &'static str,
    old_target: &ReferenceTarget,
    new_target: Option<&ReferenceTarget>,
    entity_kind: EntityKind,
) -> Result<(ReferencePath, String), ReferenceRepairError> {
    let new_target = new_target.ok_or(ReferenceRepairError::RepairWithoutNewTarget)?;
    let old_resolved = parse_reference_path(raw, referrer_modules, kind)
        .map_err(|_| ReferenceRepairError::TargetMismatch)?;
    let new_resolved = repaired_resolved_path(old_target, new_target, &old_resolved)
        .ok_or(ReferenceRepairError::TargetMismatch)?;
    let is_absolute = raw.0.starts_with('/');
    let new_raw = format_reference_path(&new_resolved, referrer_modules, is_absolute, kind);
    let commit = match entity_kind {
        EntityKind::Requirement => get_requirement(&project.tree, &new_resolved)
            .and_then(|requirement| requirement.commit.clone()),
        EntityKind::Test => get_test(&project.tree, &new_resolved).and_then(|test| test.commit.clone()),
    }
    .unwrap_or_default();
    Ok((new_raw, commit))
}

fn site_index(kind: &ReferenceSiteKind) -> usize {
    match kind {
        ReferenceSiteKind::RequirementTestReference { index } => *index,
        ReferenceSiteKind::RequirementDependency { index } => *index,
        _ => 0,
    }
}

/// Applies `actions` (paired with `ReferenceSite`s `find_references`
/// found for `old_target`) to `project`. `Repair` rewrites a reference to
/// point at `new_target` (an error if none was given); `Remove` clears the
/// reference (or, for a result's required fields, deletes the whole
/// result); `Ignore` does nothing.
///
/// List-based sites (`RequirementTestReference`/`RequirementDependency`)
/// are applied in descending index order so that removing one entry never
/// invalidates another action's index into the same list.
pub fn apply_reference_actions(
    project: &mut ProjectDraft,
    old_target: &ReferenceTarget,
    new_target: Option<&ReferenceTarget>,
    actions: &[(ReferenceSite, ReferenceAction)],
) -> Result<(), ReferenceRepairError> {
    let mut ordered: Vec<&(ReferenceSite, ReferenceAction)> = actions
        .iter()
        .filter(|(_, action)| *action != ReferenceAction::Ignore)
        .collect();
    ordered.sort_by_key(|(site, _)| Reverse(site_index(&site.kind)));

    for (site, action) in ordered {
        match &site.kind {
            ReferenceSiteKind::RequirementTestReference { index } => {
                apply_to_requirement_test_ref(project, site, *index, *action, old_target, new_target)?
            }
            ReferenceSiteKind::RequirementDependency { index } => {
                apply_to_requirement_dependency(project, site, *index, *action, old_target, new_target)?
            }
            ReferenceSiteKind::ResultTestRef { result_name } => {
                apply_to_result(project, site, result_name, *action, old_target, new_target)?
            }
        }
    }
    Ok(())
}

fn apply_to_requirement_test_ref(
    project: &mut ProjectDraft,
    site: &ReferenceSite,
    index: usize,
    action: ReferenceAction,
    old_target: &ReferenceTarget,
    new_target: Option<&ReferenceTarget>,
) -> Result<(), ReferenceRepairError> {
    let referrer_modules = site.referrer.modules.clone();
    let repaired = if action == ReferenceAction::Repair {
        let raw = {
            let requirement = get_module(&project.tree, &referrer_modules)
                .and_then(|module| module.requirements.get(&site.referrer.name))
                .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
            let test_ref = requirement.tests.get(index).ok_or_else(|| {
                ReferenceRepairError::MissingIndex {
                    referrer: site.referrer.clone(),
                    index,
                }
            })?;
            let TestReferenceKind::TestReferenceV1(local) = test_ref;
            local.path.clone()
        };
        Some(compute_repair(
            project,
            &referrer_modules,
            &raw,
            "tests",
            old_target,
            new_target,
            EntityKind::Test,
        )?)
    } else {
        None
    };

    let module = get_module_mut(&mut project.tree, &referrer_modules)
        .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
    let requirement = module
        .requirements
        .get_mut(&site.referrer.name)
        .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
    if index >= requirement.tests.len() {
        return Err(ReferenceRepairError::MissingIndex {
            referrer: site.referrer.clone(),
            index,
        });
    }
    if action == ReferenceAction::Remove {
        requirement.tests.remove(index);
    } else {
        let (path, commit) = repaired.expect("computed above for a non-Remove action");
        requirement.tests[index] = TestReferenceKind::TestReferenceV1(LocalGitReference { path, commit });
    }
    Ok(())
}

fn apply_to_requirement_dependency(
    project: &mut ProjectDraft,
    site: &ReferenceSite,
    index: usize,
    action: ReferenceAction,
    old_target: &ReferenceTarget,
    new_target: Option<&ReferenceTarget>,
) -> Result<(), ReferenceRepairError> {
    let referrer_modules = site.referrer.modules.clone();
    let repaired = if action == ReferenceAction::Repair {
        let raw = {
            let requirement = get_module(&project.tree, &referrer_modules)
                .and_then(|module| module.requirements.get(&site.referrer.name))
                .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
            let dependency = requirement.dependencies.get(index).ok_or_else(|| {
                ReferenceRepairError::MissingIndex {
                    referrer: site.referrer.clone(),
                    index,
                }
            })?;
            let DependencyReferenceKind::RequirementReferenceV1(local) = dependency else {
                return Err(ReferenceRepairError::NotALocalRequirementReference {
                    referrer: site.referrer.clone(),
                    index,
                });
            };
            local.path.clone()
        };
        Some(compute_repair(
            project,
            &referrer_modules,
            &raw,
            "requirements",
            old_target,
            new_target,
            EntityKind::Requirement,
        )?)
    } else {
        None
    };

    let module = get_module_mut(&mut project.tree, &referrer_modules)
        .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
    let requirement = module
        .requirements
        .get_mut(&site.referrer.name)
        .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
    if index >= requirement.dependencies.len() {
        return Err(ReferenceRepairError::MissingIndex {
            referrer: site.referrer.clone(),
            index,
        });
    }
    if action == ReferenceAction::Remove {
        requirement.dependencies.remove(index);
    } else {
        let (path, commit) = repaired.expect("computed above for a non-Remove action");
        requirement.dependencies[index] =
            DependencyReferenceKind::RequirementReferenceV1(LocalGitReference { path, commit });
    }
    Ok(())
}

/// Applies a `ResultTestRef` action to the result named `result_name`,
/// nested under the requirement at `site.referrer`. Unlike
/// `apply_to_requirement_test_ref`/`apply_to_requirement_dependency`, there's
/// no index to shift — a result's `test_path` is a single required field,
/// same "no empty state" reasoning as those, but with only one referrer
/// (the requirement) plus a name to pin down which of its results this is.
fn apply_to_result(
    project: &mut ProjectDraft,
    site: &ReferenceSite,
    result_name: &EntryName,
    action: ReferenceAction,
    old_target: &ReferenceTarget,
    new_target: Option<&ReferenceTarget>,
) -> Result<(), ReferenceRepairError> {
    let referrer_modules = site.referrer.modules.clone();
    let result_path = ResultPath {
        requirement: site.referrer.clone(),
        name: result_name.clone(),
    };

    if action == ReferenceAction::Remove {
        let module = get_module_mut(&mut project.tree, &referrer_modules)
            .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
        let requirement = module
            .requirements
            .get_mut(&site.referrer.name)
            .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
        return match requirement.results.remove(result_name) {
            Some(_) => Ok(()),
            None => Err(ReferenceRepairError::UnknownReferrer(site.referrer.clone())),
        };
    }

    let raw = get_result(&project.tree, &result_path)
        .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?
        .test_path
        .clone();
    let (new_path, new_commit) = compute_repair(
        project,
        &referrer_modules,
        &raw,
        "tests",
        old_target,
        new_target,
        EntityKind::Test,
    )?;

    let result = get_result_mut(&mut project.tree, &result_path)
        .ok_or_else(|| ReferenceRepairError::UnknownReferrer(site.referrer.clone()))?;
    result.test_path = new_path;
    result.test_commit = new_commit;
    Ok(())
}

#[cfg(test)]
mod test {
    use disk::ResultKindV1;

    use super::*;
    use crate::draft::{RequirementDraft, ResultDraft, TestDraft, create_project};

    fn entry(name: &str) -> EntryName {
        EntryName(name.to_string())
    }

    fn requirement(text: &str) -> RequirementDraft {
        let mut requirement = RequirementDraft::new("Title");
        requirement.requirement_text = text.to_string();
        requirement
    }

    fn test(text: &str) -> TestDraft {
        let mut test = TestDraft::new("Title", ResultKindV1::FreeForm);
        test.test_text = text.to_string();
        test
    }

    fn test_ref(path: &str, commit: &str) -> TestReferenceKind {
        TestReferenceKind::TestReferenceV1(LocalGitReference {
            path: ReferencePath(path.to_string()),
            commit: commit.to_string(),
        })
    }

    fn requirement_dep(path: &str, commit: &str) -> DependencyReferenceKind {
        DependencyReferenceKind::RequirementReferenceV1(LocalGitReference {
            path: ReferencePath(path.to_string()),
            commit: commit.to_string(),
        })
    }

    fn root_path(name: &str) -> LogicalPath {
        LogicalPath::root(entry(name))
    }

    // ---- find_references: requirement/test targets ----

    #[test]
    fn find_references_is_empty_with_no_referrers() {
        let mut project = create_project("Project");
        project
            .tree
            .add_requirement("definition", requirement("Text"))
            .unwrap();
        let sites = find_references(&project, &ReferenceTarget::Requirement(root_path("definition")));
        assert!(sites.is_empty());
    }

    #[test]
    fn find_references_finds_a_single_test_reference() {
        let mut project = create_project("Project");
        project.tree.add_test("generic_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Test(root_path("generic_test")));
        assert_eq!(
            sites,
            vec![ReferenceSite {
                referrer: root_path("definition"),
                kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
            }]
        );
    }

    #[test]
    fn find_references_finds_many_referrers_of_a_requirement() {
        let mut project = create_project("Project");
        project
            .tree
            .add_requirement("base", requirement("Text"))
            .unwrap();
        let mut dependent_a = requirement("Text");
        dependent_a.dependencies.push(requirement_dep("/requirements/base", "abc"));
        project.tree.add_requirement("a", dependent_a).unwrap();
        let mut dependent_b = requirement("Text");
        dependent_b.dependencies.push(requirement_dep("requirements/base", "abc"));
        project.tree.add_requirement("b", dependent_b).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Requirement(root_path("base")));
        assert_eq!(sites.len(), 2);
        assert!(
            sites
                .iter()
                .all(|site| site.kind == ReferenceSiteKind::RequirementDependency { index: 0 })
        );
    }

    #[test]
    fn find_references_finds_a_result_test_reference() {
        let mut project = create_project("Project");
        project
            .tree
            .add_requirement("definition", requirement("Text"))
            .unwrap();
        project.tree.add_test("generic_test", test("Text")).unwrap();
        project
            .tree
            .requirements
            .get_mut(&entry("definition"))
            .unwrap()
            .add_result(
                "result",
                ResultDraft::new(
                    "Title",
                    "abc",
                    ReferencePath("/tests/generic_test".to_string()),
                    "def",
                ),
            )
            .unwrap();

        let test_sites = find_references(&project, &ReferenceTarget::Test(root_path("generic_test")));
        assert_eq!(
            test_sites,
            vec![ReferenceSite {
                referrer: root_path("definition"),
                kind: ReferenceSiteKind::ResultTestRef {
                    result_name: entry("result"),
                },
            }]
        );

        // A result's requirement is structural now, not a tracked
        // reference — renaming/removing the requirement never shows up as
        // a `find_references` hit for it.
        let requirement_sites =
            find_references(&project, &ReferenceTarget::Requirement(root_path("definition")));
        assert!(requirement_sites.is_empty());
    }

    #[test]
    fn find_references_skips_dependencies_that_are_not_local_requirement_references() {
        let mut project = create_project("Project");
        project.tree.add_requirement("base", requirement("Text")).unwrap();
        let mut dependent = requirement("Text");
        dependent.dependencies.push(DependencyReferenceKind::Submodules);
        dependent.dependencies.push(requirement_dep("/requirements/base", "abc"));
        project.tree.add_requirement("dependent", dependent).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Requirement(root_path("base")));
        assert_eq!(
            sites,
            vec![ReferenceSite {
                referrer: root_path("dependent"),
                kind: ReferenceSiteKind::RequirementDependency { index: 1 },
            }]
        );
    }

    // ---- find_references: module targets, including the self-heal cases ----

    #[test]
    fn find_references_finds_an_absolute_reference_into_a_renamed_module() {
        let mut project = create_project("Project");
        project.tree.add_module("embeddings").unwrap();
        project
            .tree
            .modules
            .get_mut(&entry("embeddings"))
            .unwrap()
            .add_test("generic_test", test("Text"))
            .unwrap();
        let mut requirement = requirement("Text");
        requirement
            .tests
            .push(test_ref("/modules/embeddings/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Module(vec![entry("embeddings")]));
        assert_eq!(
            sites,
            vec![ReferenceSite {
                referrer: root_path("definition"),
                kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
            }]
        );
    }

    #[test]
    fn find_references_finds_a_relative_dip_into_a_renamed_submodule() {
        let mut project = create_project("Project");
        project.tree.add_module("embeddings").unwrap();
        project
            .tree
            .modules
            .get_mut(&entry("embeddings"))
            .unwrap()
            .add_test("generic_test", test("Text"))
            .unwrap();
        let mut requirement = requirement("Text");
        requirement
            .tests
            .push(test_ref("modules/embeddings/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Module(vec![entry("embeddings")]));
        assert_eq!(sites.len(), 1);
    }

    #[test]
    fn find_references_does_not_flag_a_relative_reference_whose_module_is_only_an_implicit_ancestor() {
        // `definition` lives inside `embeddings` and references
        // `generic_test` (also inside `embeddings`) relatively — the
        // string itself never spells out "embeddings", so renaming
        // `embeddings` doesn't touch it: it self-heals via the live tree
        // the next time it's resolved.
        let mut project = create_project("Project");
        project.tree.add_module("embeddings").unwrap();
        let embeddings = project.tree.modules.get_mut(&entry("embeddings")).unwrap();
        embeddings.add_test("generic_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("tests/generic_test", "abc"));
        embeddings.add_requirement("definition", requirement).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Module(vec![entry("embeddings")]));
        assert!(sites.is_empty());
    }

    // ---- apply_reference_actions: Remove ----

    #[test]
    fn remove_clears_a_test_reference_from_the_list() {
        let mut project = create_project("Project");
        project.tree.add_test("generic_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let old_target = ReferenceTarget::Test(root_path("generic_test"));
        let sites = find_references(&project, &old_target);
        apply_reference_actions(&mut project, &old_target, None, &[(sites[0].clone(), ReferenceAction::Remove)])
            .unwrap();

        assert!(
            project
                .tree
                .requirements
                .get(&entry("definition"))
                .unwrap()
                .tests
                .is_empty()
        );
    }

    #[test]
    fn remove_clears_a_dependency_from_the_list() {
        let mut project = create_project("Project");
        project.tree.add_requirement("base", requirement("Text")).unwrap();
        let mut dependent = requirement("Text");
        dependent.dependencies.push(requirement_dep("/requirements/base", "abc"));
        project.tree.add_requirement("dependent", dependent).unwrap();

        let old_target = ReferenceTarget::Requirement(root_path("base"));
        let sites = find_references(&project, &old_target);
        apply_reference_actions(&mut project, &old_target, None, &[(sites[0].clone(), ReferenceAction::Remove)])
            .unwrap();

        assert!(
            project
                .tree
                .requirements
                .get(&entry("dependent"))
                .unwrap()
                .dependencies
                .is_empty()
        );
    }

    #[test]
    fn remove_deletes_the_whole_result_for_a_result_test_ref() {
        let mut project = create_project("Project");
        project.tree.add_requirement("definition", requirement("Text")).unwrap();
        project.tree.add_test("generic_test", test("Text")).unwrap();
        let definition = project.tree.requirements.get_mut(&entry("definition")).unwrap();
        definition
            .add_result(
                "result",
                ResultDraft::new(
                    "Title",
                    "abc",
                    ReferencePath("/tests/generic_test".to_string()),
                    "def",
                ),
            )
            .unwrap();

        let old_target = ReferenceTarget::Test(root_path("generic_test"));
        let sites = find_references(&project, &old_target);
        apply_reference_actions(&mut project, &old_target, None, &[(sites[0].clone(), ReferenceAction::Remove)])
            .unwrap();

        assert!(
            project
                .tree
                .requirements
                .get(&entry("definition"))
                .unwrap()
                .results
                .is_empty()
        );
    }

    #[test]
    fn remove_of_two_dependencies_on_the_same_requirement_does_not_shift_indices() {
        let mut project = create_project("Project");
        project.tree.add_requirement("base", requirement("Text")).unwrap();
        let mut dependent = requirement("Text");
        dependent.dependencies.push(requirement_dep("/requirements/base", "abc"));
        dependent.dependencies.push(requirement_dep("/requirements/other", "abc"));
        dependent.dependencies.push(requirement_dep("/requirements/base", "abc"));
        project.tree.add_requirement("dependent", dependent).unwrap();

        let old_target = ReferenceTarget::Requirement(root_path("base"));
        let sites = find_references(&project, &old_target);
        assert_eq!(sites.len(), 2);
        let actions: Vec<_> = sites
            .into_iter()
            .map(|site| (site, ReferenceAction::Remove))
            .collect();
        apply_reference_actions(&mut project, &old_target, None, &actions).unwrap();

        let remaining = &project.tree.requirements.get(&entry("dependent")).unwrap().dependencies;
        assert_eq!(remaining.len(), 1);
        assert!(matches!(
            &remaining[0],
            DependencyReferenceKind::RequirementReferenceV1(local) if local.path.0 == "/requirements/other"
        ));
    }

    // ---- apply_reference_actions: Repair ----

    #[test]
    fn repair_rewrites_an_absolute_test_reference_and_refreshes_the_commit() {
        let mut project = create_project("Project");
        let mut renamed_test = test("Text");
        renamed_test.commit = Some("new-commit".to_string());
        project.tree.add_test("renamed_test", renamed_test).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("/tests/generic_test", "stale-commit"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let old_target = ReferenceTarget::Test(root_path("generic_test"));
        let new_target = ReferenceTarget::Test(root_path("renamed_test"));
        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
        };
        apply_reference_actions(
            &mut project,
            &old_target,
            Some(&new_target),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap();

        let TestReferenceKind::TestReferenceV1(local) =
            &project.tree.requirements.get(&entry("definition")).unwrap().tests[0];
        assert_eq!(local.path.0, "/tests/renamed_test");
        assert_eq!(local.commit, "new-commit");
    }

    #[test]
    fn repair_preserves_a_relative_style_reference() {
        let mut project = create_project("Project");
        project.tree.add_test("renamed_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let old_target = ReferenceTarget::Test(root_path("generic_test"));
        let new_target = ReferenceTarget::Test(root_path("renamed_test"));
        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
        };
        apply_reference_actions(
            &mut project,
            &old_target,
            Some(&new_target),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap();

        let TestReferenceKind::TestReferenceV1(local) =
            &project.tree.requirements.get(&entry("definition")).unwrap().tests[0];
        assert_eq!(local.path.0, "tests/renamed_test");
    }

    #[test]
    fn repair_leaves_the_commit_empty_when_the_new_target_has_never_been_saved() {
        let mut project = create_project("Project");
        project.tree.add_test("renamed_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let old_target = ReferenceTarget::Test(root_path("generic_test"));
        let new_target = ReferenceTarget::Test(root_path("renamed_test"));
        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
        };
        apply_reference_actions(
            &mut project,
            &old_target,
            Some(&new_target),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap();

        let TestReferenceKind::TestReferenceV1(local) =
            &project.tree.requirements.get(&entry("definition")).unwrap().tests[0];
        assert_eq!(local.commit, "");
    }

    #[test]
    fn repair_rewrites_a_requirement_dependency() {
        let mut project = create_project("Project");
        project.tree.add_requirement("renamed_base", requirement("Text")).unwrap();
        let mut dependent = requirement("Text");
        dependent.dependencies.push(requirement_dep("/requirements/base", "abc"));
        project.tree.add_requirement("dependent", dependent).unwrap();

        let old_target = ReferenceTarget::Requirement(root_path("base"));
        let new_target = ReferenceTarget::Requirement(root_path("renamed_base"));
        let site = ReferenceSite {
            referrer: root_path("dependent"),
            kind: ReferenceSiteKind::RequirementDependency { index: 0 },
        };
        apply_reference_actions(
            &mut project,
            &old_target,
            Some(&new_target),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap();

        assert!(matches!(
            &project.tree.requirements.get(&entry("dependent")).unwrap().dependencies[0],
            DependencyReferenceKind::RequirementReferenceV1(local) if local.path.0 == "/requirements/renamed_base"
        ));
    }

    #[test]
    fn repair_rewrites_a_result_test_ref() {
        let mut project = create_project("Project");
        project.tree.add_requirement("definition", requirement("Text")).unwrap();
        project.tree.add_test("renamed_test", test("Text")).unwrap();
        project
            .tree
            .requirements
            .get_mut(&entry("definition"))
            .unwrap()
            .add_result(
                "result",
                ResultDraft::new(
                    "Title",
                    "abc",
                    ReferencePath("/tests/generic_test".to_string()),
                    "def",
                ),
            )
            .unwrap();

        apply_reference_actions(
            &mut project,
            &ReferenceTarget::Test(root_path("generic_test")),
            Some(&ReferenceTarget::Test(root_path("renamed_test"))),
            &[(
                ReferenceSite {
                    referrer: root_path("definition"),
                    kind: ReferenceSiteKind::ResultTestRef {
                        result_name: entry("result"),
                    },
                },
                ReferenceAction::Repair,
            )],
        )
        .unwrap();

        let result = project
            .tree
            .requirements
            .get(&entry("definition"))
            .unwrap()
            .results
            .get(&entry("result"))
            .unwrap();
        assert_eq!(result.test_path.0, "/tests/renamed_test");
    }

    #[test]
    fn repair_of_a_module_rename_rewrites_a_descendant_reference() {
        let mut project = create_project("Project");
        project.tree.add_module("renamed").unwrap();
        project
            .tree
            .modules
            .get_mut(&entry("renamed"))
            .unwrap()
            .add_test("generic_test", test("Text"))
            .unwrap();
        let mut requirement = requirement("Text");
        requirement
            .tests
            .push(test_ref("/modules/embeddings/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let old_target = ReferenceTarget::Module(vec![entry("embeddings")]);
        let new_target = ReferenceTarget::Module(vec![entry("renamed")]);
        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
        };
        apply_reference_actions(
            &mut project,
            &old_target,
            Some(&new_target),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap();

        let TestReferenceKind::TestReferenceV1(local) =
            &project.tree.requirements.get(&entry("definition")).unwrap().tests[0];
        assert_eq!(local.path.0, "/modules/renamed/tests/generic_test");
    }

    // ---- Ignore ----

    #[test]
    fn ignore_leaves_the_reference_untouched() {
        let mut project = create_project("Project");
        project.tree.add_test("generic_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let old_target = ReferenceTarget::Test(root_path("generic_test"));
        let sites = find_references(&project, &old_target);
        apply_reference_actions(&mut project, &old_target, None, &[(sites[0].clone(), ReferenceAction::Ignore)])
            .unwrap();

        let TestReferenceKind::TestReferenceV1(local) =
            &project.tree.requirements.get(&entry("definition")).unwrap().tests[0];
        assert_eq!(local.path.0, "/tests/generic_test");
    }

    // ---- error paths ----

    #[test]
    fn repair_without_a_new_target_is_an_error() {
        let mut project = create_project("Project");
        project.tree.add_test("generic_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let old_target = ReferenceTarget::Test(root_path("generic_test"));
        let sites = find_references(&project, &old_target);
        let err = apply_reference_actions(&mut project, &old_target, None, &[(sites[0].clone(), ReferenceAction::Repair)])
            .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::RepairWithoutNewTarget));
    }

    #[test]
    fn unknown_referrer_is_an_error() {
        let mut project = create_project("Project");
        let site = ReferenceSite {
            referrer: root_path("nonexistent"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Test(root_path("generic_test")),
            None,
            &[(site, ReferenceAction::Remove)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::UnknownReferrer(_)));
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn missing_index_is_an_error() {
        let mut project = create_project("Project");
        project.tree.add_requirement("definition", requirement("Text")).unwrap();
        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 5 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Test(root_path("generic_test")),
            None,
            &[(site, ReferenceAction::Remove)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::MissingIndex { .. }));
    }

    #[test]
    fn not_a_local_requirement_reference_is_an_error_when_repairing() {
        let mut project = create_project("Project");
        let mut dependent = requirement("Text");
        dependent.dependencies.push(DependencyReferenceKind::Submodules);
        project.tree.add_requirement("dependent", dependent).unwrap();

        let site = ReferenceSite {
            referrer: root_path("dependent"),
            kind: ReferenceSiteKind::RequirementDependency { index: 0 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Requirement(root_path("base")),
            Some(&ReferenceTarget::Requirement(root_path("renamed_base"))),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::NotALocalRequirementReference { .. }));
    }

    #[test]
    fn repair_with_a_missing_test_reference_index_is_an_error() {
        let mut project = create_project("Project");
        project.tree.add_requirement("definition", requirement("Text")).unwrap();
        project.tree.add_test("renamed_test", test("Text")).unwrap();

        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 5 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Test(root_path("generic_test")),
            Some(&ReferenceTarget::Test(root_path("renamed_test"))),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::MissingIndex { .. }));
    }

    #[test]
    fn repair_with_a_missing_dependency_index_is_an_error() {
        let mut project = create_project("Project");
        project.tree.add_requirement("dependent", requirement("Text")).unwrap();
        project.tree.add_requirement("renamed_base", requirement("Text")).unwrap();

        let site = ReferenceSite {
            referrer: root_path("dependent"),
            kind: ReferenceSiteKind::RequirementDependency { index: 5 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Requirement(root_path("base")),
            Some(&ReferenceTarget::Requirement(root_path("renamed_base"))),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::MissingIndex { .. }));
    }

    #[test]
    fn missing_index_is_an_error_for_a_dependency_remove() {
        let mut project = create_project("Project");
        project.tree.add_requirement("dependent", requirement("Text")).unwrap();
        let site = ReferenceSite {
            referrer: root_path("dependent"),
            kind: ReferenceSiteKind::RequirementDependency { index: 5 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Requirement(root_path("base")),
            None,
            &[(site, ReferenceAction::Remove)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::MissingIndex { .. }));
    }

    #[test]
    fn repair_of_a_dependency_without_a_new_target_is_an_error() {
        let mut project = create_project("Project");
        let mut dependent = requirement("Text");
        dependent.dependencies.push(requirement_dep("/requirements/base", "abc"));
        project.tree.add_requirement("dependent", dependent).unwrap();

        let site = ReferenceSite {
            referrer: root_path("dependent"),
            kind: ReferenceSiteKind::RequirementDependency { index: 0 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Requirement(root_path("base")),
            None,
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::RepairWithoutNewTarget));
    }

    #[test]
    fn repair_of_a_result_reference_without_a_new_target_is_an_error() {
        let mut project = create_project("Project");
        project.tree.add_requirement("definition", requirement("Text")).unwrap();
        project.tree.add_test("generic_test", test("Text")).unwrap();
        project
            .tree
            .requirements
            .get_mut(&entry("definition"))
            .unwrap()
            .add_result(
                "result",
                ResultDraft::new(
                    "Title",
                    "abc",
                    ReferencePath("/tests/generic_test".to_string()),
                    "def",
                ),
            )
            .unwrap();

        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::ResultTestRef {
                result_name: entry("result"),
            },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Test(root_path("generic_test")),
            None,
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::RepairWithoutNewTarget));
    }

    #[test]
    fn removing_a_result_that_does_not_exist_is_an_error() {
        let mut project = create_project("Project");
        let site = ReferenceSite {
            referrer: root_path("nonexistent_requirement"),
            kind: ReferenceSiteKind::ResultTestRef {
                result_name: entry("nonexistent_result"),
            },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Test(root_path("generic_test")),
            None,
            &[(site, ReferenceAction::Remove)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::UnknownReferrer(_)));
    }

    #[test]
    fn target_mismatch_is_an_error_when_old_and_new_targets_are_different_kinds() {
        let mut project = create_project("Project");
        project.tree.add_test("generic_test", test("Text")).unwrap();
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Test(root_path("generic_test")),
            Some(&ReferenceTarget::Requirement(root_path("renamed"))),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::TargetMismatch));
    }

    #[test]
    fn repair_of_a_module_rename_is_a_target_mismatch_outside_the_old_prefix() {
        // The site claims to be a reference into `embeddings`, but the
        // reference it actually names resolves to a different module
        // entirely — `repaired_resolved_path`'s module-rename arm only
        // applies when the resolved path truly starts with the old prefix.
        let mut project = create_project("Project");
        project.tree.add_module("other").unwrap();
        project
            .tree
            .modules
            .get_mut(&entry("other"))
            .unwrap()
            .add_test("generic_test", test("Text"))
            .unwrap();
        let mut requirement = requirement("Text");
        requirement
            .tests
            .push(test_ref("/modules/other/tests/generic_test", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let site = ReferenceSite {
            referrer: root_path("definition"),
            kind: ReferenceSiteKind::RequirementTestReference { index: 0 },
        };
        let err = apply_reference_actions(
            &mut project,
            &ReferenceTarget::Module(vec![entry("embeddings")]),
            Some(&ReferenceTarget::Module(vec![entry("renamed")])),
            &[(site, ReferenceAction::Repair)],
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceRepairError::TargetMismatch));
    }

    // ---- find_references: malformed references are skipped, not errors ----

    #[test]
    fn find_references_skips_a_malformed_test_reference() {
        let mut project = create_project("Project");
        let mut requirement = requirement("Text");
        requirement.tests.push(test_ref("bogus", "abc"));
        project.tree.add_requirement("definition", requirement).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Test(root_path("generic_test")));
        assert!(sites.is_empty());
    }

    #[test]
    fn find_references_skips_a_malformed_dependency_reference() {
        let mut project = create_project("Project");
        let mut dependent = requirement("Text");
        dependent.dependencies.push(requirement_dep("bogus", "abc"));
        project.tree.add_requirement("dependent", dependent).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Requirement(root_path("base")));
        assert!(sites.is_empty());
    }

    #[test]
    fn find_references_skips_a_malformed_result_test_reference() {
        let mut project = create_project("Project");
        project.tree.add_requirement("definition", requirement("Text")).unwrap();
        project
            .tree
            .requirements
            .get_mut(&entry("definition"))
            .unwrap()
            .add_result(
                "result",
                ResultDraft::new("Title", "abc", ReferencePath("bogus".to_string()), "def"),
            )
            .unwrap();

        let sites = find_references(&project, &ReferenceTarget::Test(root_path("generic_test")));
        assert!(sites.is_empty());
    }

    #[test]
    fn find_references_does_not_flag_a_root_level_relative_reference_for_a_module_target() {
        // `dependent` lives at the project root and references `base`
        // (also at the root) relatively, with no module segments at all —
        // its resolved module path is shorter than `embeddings`'s prefix,
        // so renaming `embeddings` can't possibly be what broke it.
        let mut project = create_project("Project");
        project.tree.add_module("embeddings").unwrap();
        project.tree.add_requirement("base", requirement("Text")).unwrap();
        let mut dependent = requirement("Text");
        dependent.dependencies.push(requirement_dep("requirements/base", "abc"));
        project.tree.add_requirement("dependent", dependent).unwrap();

        let sites = find_references(&project, &ReferenceTarget::Module(vec![entry("embeddings")]));
        assert!(sites.is_empty());
    }
}
