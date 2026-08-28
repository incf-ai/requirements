use std::collections::BTreeSet;
use std::path::Path;

use disk::{DependencyReferenceKind, StatusV1, TestReferenceKind};
use syscalls::Filesystem;

use crate::LogicalPath;
use crate::convert::export::export_project;
use crate::draft::{ModuleDraft, ProjectDraft, ResultDraft};
use crate::lookup::{get_module, get_requirement, get_test};
use crate::path::parse_reference_path;

/// The result of successfully validating a `ProjectDraft` — see
/// `crates/logical/README.md`'s "Draft vs. validated." Just a wrapper
/// around the (now-confirmed-consistent) draft: per "Decisions made" #4,
/// nothing about reference resolution is cached here — every query below
/// re-resolves fresh against the wrapped draft.
#[derive(Debug, Clone)]
pub struct ValidatedProject(ProjectDraft);

impl ValidatedProject {
    pub(crate) fn new(draft: ProjectDraft) -> Self {
        ValidatedProject(draft)
    }

    /// Hands back a mutable draft — the only way to change a
    /// `ValidatedProject` is to go back through `validate::validate`.
    pub fn into_draft(self) -> ProjectDraft {
        self.0
    }

    pub fn draft(&self) -> &ProjectDraft {
        &self.0
    }

    /// Saves to disk via `disk::save_project`. Only a `ValidatedProject`
    /// can do this — a `ProjectDraft` has no `save` at all, so "validate
    /// before saving" is enforced by the type system, not a runtime check.
    pub fn save(
        &self,
        fs: &dyn Filesystem,
        dir: &Path,
    ) -> Result<(), disk::project::operations::save::Error> {
        let on_disk = export_project(&self.0);
        disk::save_project(fs, dir, &on_disk)
    }

    /// See `crates/logical/README.md`'s "Requirement-met semantics" for
    /// the full specification: a requirement is met when it has one or
    /// more tests, and for *every* one of them: the requirement's own
    /// reference to that test is current (its recorded commit matches the
    /// test's currently-computed commit), and some result exists whose
    /// `requirement_commit`/`test_commit` both match the current commits
    /// and whose `status` is `Pass`. Historical/non-`Pass` results are not
    /// errors — they're just excluded here, per the README.
    pub fn is_requirement_met(&self, requirement: &LogicalPath) -> bool {
        let Some(req) = get_requirement(&self.0.tree, requirement) else {
            return false;
        };
        if req.tests.is_empty() {
            return false;
        }
        let Some(req_commit) = &req.commit else {
            return false;
        };

        let all_results = collect_results(&self.0.tree, &[]);

        req.tests.iter().all(|test_ref| {
            let TestReferenceKind::TestReferenceV1(local) = test_ref;
            let Ok(target) = parse_reference_path(&local.path, &requirement.modules, "tests")
            else {
                return false;
            };
            let Some(test) = get_test(&self.0.tree, &target) else {
                return false;
            };
            let Some(test_commit) = &test.commit else {
                return false;
            };
            if &local.commit != test_commit {
                return false;
            }

            all_results.iter().any(|(result_path, result)| {
                result_satisfies(
                    result,
                    &result_path.modules,
                    requirement,
                    req_commit,
                    &target,
                    test_commit,
                )
            })
        })
    }

    /// The transitive closure of local (`RequirementReferenceV1`)
    /// dependency targets reachable from `requirement`, in DFS-discovery
    /// order, deduplicated. `RemoteReferenceV1` and `Submodules`
    /// dependencies aren't expanded — see `crates/logical/README.md`'s
    /// cycle-detection scope note: this crate's dependency graph is local
    /// edges only.
    pub fn dependency_chain(&self, requirement: &LogicalPath) -> Vec<LogicalPath> {
        let mut visited = BTreeSet::new();
        let mut order = Vec::new();
        self.walk_dependencies(requirement, &mut visited, &mut order);
        order
    }

    fn walk_dependencies(
        &self,
        path: &LogicalPath,
        visited: &mut BTreeSet<LogicalPath>,
        order: &mut Vec<LogicalPath>,
    ) {
        // No early "already visited" return here — the loop below only
        // ever recurses into a target it just confirmed (via
        // `!visited.contains`) isn't in `visited` yet, so this is only
        // ever called with a fresh path. `visited` still needs populating
        // (both here and for the very first call from `dependency_chain`)
        // so that check keeps working for later siblings/ancestors.
        visited.insert(path.clone());
        let Some(requirement) = get_requirement(&self.0.tree, path) else {
            return;
        };
        for dependency in &requirement.dependencies {
            if let DependencyReferenceKind::RequirementReferenceV1(local) = dependency
                && let Ok(target) = parse_reference_path(&local.path, &path.modules, "requirements")
                && get_requirement(&self.0.tree, &target).is_some()
                && !visited.contains(&target)
            {
                order.push(target.clone());
                self.walk_dependencies(&target, visited, order);
            }
        }
    }

    /// Every requirement in the entire (transitive) submodule subtree of
    /// `module` is met — see "Validation questions — answered" #3. Used to
    /// evaluate a bare `Submodules` dependency.
    pub fn all_requirements_met_in_subtree(&self, module: &[disk::EntryName]) -> bool {
        let Some(root) = get_module(&self.0.tree, module) else {
            return false;
        };
        self.all_requirements_met_in(root, module)
    }

    fn all_requirements_met_in(&self, module: &ModuleDraft, prefix: &[disk::EntryName]) -> bool {
        for name in module.requirements.keys() {
            let path = LogicalPath {
                modules: prefix.to_vec(),
                name: name.clone(),
            };
            if !self.is_requirement_met(&path) {
                return false;
            }
        }
        for (name, submodule) in &module.modules {
            let mut child_prefix = prefix.to_vec();
            child_prefix.push(name.clone());
            if !self.all_requirements_met_in(submodule, &child_prefix) {
                return false;
            }
        }
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn result_satisfies(
    result: &ResultDraft,
    result_module: &[disk::EntryName],
    requirement: &LogicalPath,
    requirement_commit: &str,
    test: &LogicalPath,
    test_commit: &str,
) -> bool {
    if !matches!(result.status, StatusV1::Pass) {
        return false;
    }
    if result.requirement_commit != requirement_commit || result.test_commit != test_commit {
        return false;
    }
    let Ok(result_requirement) =
        parse_reference_path(&result.requirement_path, result_module, "requirements")
    else {
        return false;
    };
    if &result_requirement != requirement {
        return false;
    }
    let Ok(result_test) = parse_reference_path(&result.test_path, result_module, "tests") else {
        return false;
    };
    &result_test == test
}

fn collect_results<'a>(
    module: &'a ModuleDraft,
    prefix: &[disk::EntryName],
) -> Vec<(LogicalPath, &'a ResultDraft)> {
    let mut out = Vec::new();
    collect_results_into(module, prefix, &mut out);
    out
}

fn collect_results_into<'a>(
    module: &'a ModuleDraft,
    prefix: &[disk::EntryName],
    out: &mut Vec<(LogicalPath, &'a ResultDraft)>,
) {
    for (name, result) in &module.results {
        out.push((
            LogicalPath {
                modules: prefix.to_vec(),
                name: name.clone(),
            },
            result,
        ));
    }
    for (name, submodule) in &module.modules {
        let mut child_prefix = prefix.to_vec();
        child_prefix.push(name.clone());
        collect_results_into(submodule, &child_prefix, out);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::draft::{RequirementDraft, TestDraft, create_project};
    use crate::test_support::FixedRemoteGit;
    use crate::validate::validate;
    use disk::{LocalGitReference, ReferencePath, ResultKindV1};

    /// Requirement "definition" -> test "generic_test", both persisted at
    /// commit "c1"/"t1" — the currency conditions `is_requirement_met`
    /// checks. No result yet, so not met.
    fn project_with_current_requirement_and_test() -> ProjectDraft {
        let mut project = create_project("Capstone");

        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .tests
            .push(TestReferenceKind::TestReferenceV1(LocalGitReference {
                path: ReferencePath("/tests/generic_test".to_string()),
                commit: "t1".to_string(),
            }));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        let mut test = TestDraft::new("Generic Test", ResultKindV1::FreeForm);
        test.commit = Some("t1".to_string());
        project.tree.add_test("generic_test", test).unwrap();

        project
    }

    fn passing_result() -> ResultDraft {
        let mut result = ResultDraft::new(
            "Definition",
            ReferencePath("requirements/definition".to_string()),
            "c1",
            ReferencePath("/tests/generic_test".to_string()),
            "t1",
        );
        result.status = StatusV1::Pass;
        result
    }

    fn validated(project: ProjectDraft) -> ValidatedProject {
        validate(project, &FixedRemoteGit).unwrap()
    }

    fn requirement_path() -> LogicalPath {
        LogicalPath::root(disk::EntryName("definition".to_string()))
    }

    #[test]
    fn not_met_with_no_result_at_all() {
        let project = validated(project_with_current_requirement_and_test());
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn met_with_a_current_passing_result() {
        let mut project = project_with_current_requirement_and_test();
        project
            .tree
            .add_result("definition", passing_result())
            .unwrap();

        let project = validated(project);
        assert!(project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_result_status_is_not_pass() {
        let mut project = project_with_current_requirement_and_test();
        let mut result = passing_result();
        result.status = StatusV1::Fail;
        project.tree.add_result("definition", result).unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_result_is_for_an_older_requirement_commit() {
        let mut project = project_with_current_requirement_and_test();
        let mut result = passing_result();
        result.requirement_commit = "stale".to_string();
        project.tree.add_result("definition", result).unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_result_is_for_an_older_test_commit() {
        let mut project = project_with_current_requirement_and_test();
        let mut result = passing_result();
        result.test_commit = "stale".to_string();
        project.tree.add_result("definition", result).unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_requirements_own_test_reference_is_stale() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        // References an old version of the test — "stale" != test.commit.
        requirement
            .tests
            .push(TestReferenceKind::TestReferenceV1(LocalGitReference {
                path: ReferencePath("/tests/generic_test".to_string()),
                commit: "stale".to_string(),
            }));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        let mut test = TestDraft::new("Generic Test", ResultKindV1::FreeForm);
        test.commit = Some("t1".to_string());
        project.tree.add_test("generic_test", test).unwrap();
        project
            .tree
            .add_result("definition", passing_result())
            .unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_with_no_tests_at_all() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_for_a_requirement_that_was_never_persisted() {
        let mut project = project_with_current_requirement_and_test();
        project
            .tree
            .add_result("definition", passing_result())
            .unwrap();
        // Simulate an in-memory-only requirement: no commit yet.
        project
            .tree
            .requirements
            .get_mut(&disk::EntryName("definition".to_string()))
            .unwrap()
            .commit = None;

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn is_requirement_met_returns_false_for_an_unknown_path() {
        let project = validated(project_with_current_requirement_and_test());
        let unknown = LogicalPath::root(disk::EntryName("nonexistent".to_string()));
        assert!(!project.is_requirement_met(&unknown));
    }

    #[test]
    fn not_met_when_the_test_reference_is_malformed() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .tests
            .push(TestReferenceKind::TestReferenceV1(LocalGitReference {
                path: ReferencePath("tests".to_string()),
                commit: "t1".to_string(),
            }));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        // Not calling `validate()` — a malformed reference is exactly what
        // it rejects. See `not_met_when_the_referenced_test_does_not_exist`
        // for why `is_requirement_met` still needs to handle this.
        let project = ValidatedProject::new(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_referenced_test_does_not_exist() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .tests
            .push(TestReferenceKind::TestReferenceV1(LocalGitReference {
                path: ReferencePath("/tests/nonexistent".to_string()),
                commit: "t1".to_string(),
            }));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        // Not calling `validate()` here — an unresolved reference would be
        // rejected there. `is_requirement_met` needs to handle it gracefully
        // too, since a `ValidatedProject` could still be reached via
        // `into_draft()` -> re-edit -> a *different* validate() pass that
        // doesn't happen to re-check this exact requirement.
        let project = ValidatedProject::new(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_referenced_test_was_never_persisted() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .tests
            .push(TestReferenceKind::TestReferenceV1(LocalGitReference {
                path: ReferencePath("/tests/generic_test".to_string()),
                commit: "t1".to_string(),
            }));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();
        // No `commit` set — never persisted.
        project
            .tree
            .add_test(
                "generic_test",
                TestDraft::new("Generic Test", ResultKindV1::FreeForm),
            )
            .unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_result_names_a_different_requirement() {
        let mut project = project_with_current_requirement_and_test();
        let mut requirement = RequirementDraft::new("Other");
        requirement.commit = Some("c1".to_string());
        project.tree.add_requirement("other", requirement).unwrap();

        let mut result = passing_result();
        // Same commit as the real requirement, but names a different one.
        result.requirement_path = ReferencePath("requirements/other".to_string());
        project.tree.add_result("definition", result).unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_results_requirement_reference_is_malformed() {
        let mut project = project_with_current_requirement_and_test();
        let mut result = passing_result();
        result.requirement_path = ReferencePath("requirements".to_string());
        project.tree.add_result("definition", result).unwrap();

        let project = validated(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn not_met_when_the_results_test_reference_is_malformed() {
        let mut project = project_with_current_requirement_and_test();
        let mut result = passing_result();
        result.test_path = ReferencePath("tests".to_string());
        project.tree.add_result("definition", result).unwrap();

        let project = ValidatedProject::new(project);
        assert!(!project.is_requirement_met(&requirement_path()));
    }

    #[test]
    fn met_with_a_result_and_a_met_submodule_requirement() {
        let mut project = create_project("Capstone");
        project.tree.add_module("embeddings").unwrap();
        let submodule = project
            .tree
            .modules
            .get_mut(&disk::EntryName("embeddings".to_string()))
            .unwrap();

        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .tests
            .push(TestReferenceKind::TestReferenceV1(LocalGitReference {
                path: ReferencePath("tests/generic_test".to_string()),
                commit: "t1".to_string(),
            }));
        submodule
            .add_requirement("definition", requirement)
            .unwrap();

        let mut test = TestDraft::new("Generic Test", ResultKindV1::FreeForm);
        test.commit = Some("t1".to_string());
        submodule.add_test("generic_test", test).unwrap();

        let mut result = ResultDraft::new(
            "Definition",
            ReferencePath("requirements/definition".to_string()),
            "c1",
            ReferencePath("tests/generic_test".to_string()),
            "t1",
        );
        result.status = StatusV1::Pass;
        submodule.add_result("definition", result).unwrap();

        let project = validated(project);
        let path = LogicalPath {
            modules: vec![disk::EntryName("embeddings".to_string())],
            name: disk::EntryName("definition".to_string()),
        };
        assert!(project.is_requirement_met(&path));
        assert!(project.all_requirements_met_in_subtree(&[]));
    }

    #[test]
    fn dependency_chain_follows_local_edges_transitively() {
        let mut project = create_project("Capstone");
        for (name, target) in [("a", Some("b")), ("b", Some("c")), ("c", None)] {
            let mut requirement = RequirementDraft::new(name);
            requirement.commit = Some("c1".to_string());
            if let Some(target) = target {
                requirement
                    .dependencies
                    .push(DependencyReferenceKind::RequirementReferenceV1(
                        LocalGitReference {
                            path: ReferencePath(format!("requirements/{target}")),
                            commit: "c1".to_string(),
                        },
                    ));
            }
            project.tree.add_requirement(name, requirement).unwrap();
        }

        let project = validated(project);
        let chain = project.dependency_chain(&LogicalPath::root(disk::EntryName("a".to_string())));
        assert_eq!(
            chain,
            vec![
                LogicalPath::root(disk::EntryName("b".to_string())),
                LogicalPath::root(disk::EntryName("c".to_string())),
            ]
        );
    }

    #[test]
    fn dependency_chain_deduplicates_a_diamond() {
        let mut project = create_project("Capstone");
        // a -> b, a -> c, b -> d, c -> d: "d" is reachable two ways.
        let edges: &[(&str, &[&str])] =
            &[("a", &["b", "c"]), ("b", &["d"]), ("c", &["d"]), ("d", &[])];
        for (name, targets) in edges {
            let mut requirement = RequirementDraft::new(*name);
            requirement.commit = Some("c1".to_string());
            for target in *targets {
                requirement
                    .dependencies
                    .push(DependencyReferenceKind::RequirementReferenceV1(
                        LocalGitReference {
                            path: ReferencePath(format!("requirements/{target}")),
                            commit: "c1".to_string(),
                        },
                    ));
            }
            project.tree.add_requirement(*name, requirement).unwrap();
        }

        let project = validated(project);
        let chain = project.dependency_chain(&LogicalPath::root(disk::EntryName("a".to_string())));
        assert_eq!(chain.len(), 3);
        assert!(chain.contains(&LogicalPath::root(disk::EntryName("d".to_string()))));
    }

    #[test]
    fn dependency_chain_is_empty_for_a_requirement_with_no_dependencies() {
        let project = validated(project_with_current_requirement_and_test());
        assert!(project.dependency_chain(&requirement_path()).is_empty());
    }

    #[test]
    fn dependency_chain_is_empty_for_an_unknown_path() {
        let project = validated(project_with_current_requirement_and_test());
        let unknown = LogicalPath::root(disk::EntryName("nonexistent".to_string()));
        assert!(project.dependency_chain(&unknown).is_empty());
    }

    #[test]
    fn dependency_chain_skips_a_dependency_that_does_not_resolve() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("A");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::RequirementReferenceV1(
                LocalGitReference {
                    path: ReferencePath("requirements/nonexistent".to_string()),
                    commit: "c1".to_string(),
                },
            ));
        project.tree.add_requirement("a", requirement).unwrap();

        // Not calling `validate()` — an unresolved dependency is exactly
        // what it rejects; `dependency_chain` still needs to cope with one
        // reaching it some other way (see `is_requirement_met`'s similar
        // tests for why).
        let project = ValidatedProject::new(project);
        assert!(
            project
                .dependency_chain(&LogicalPath::root(disk::EntryName("a".to_string())))
                .is_empty()
        );
    }

    #[test]
    fn dependency_chain_skips_a_malformed_dependency_reference() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("A");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::RequirementReferenceV1(
                LocalGitReference {
                    path: ReferencePath("requirements".to_string()),
                    commit: "c1".to_string(),
                },
            ));
        project.tree.add_requirement("a", requirement).unwrap();

        let project = ValidatedProject::new(project);
        assert!(
            project
                .dependency_chain(&LogicalPath::root(disk::EntryName("a".to_string())))
                .is_empty()
        );
    }

    #[test]
    fn dependency_chain_skips_non_local_dependencies() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("A");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::Submodules);
        project.tree.add_requirement("a", requirement).unwrap();

        let project = validated(project);
        assert!(
            project
                .dependency_chain(&LogicalPath::root(disk::EntryName("a".to_string())))
                .is_empty()
        );
    }

    #[test]
    fn all_requirements_met_in_subtree_is_true_for_an_empty_module() {
        let project = validated(create_project("Capstone"));
        assert!(project.all_requirements_met_in_subtree(&[]));
    }

    #[test]
    fn all_requirements_met_in_subtree_checks_nested_submodules() {
        let mut project = create_project("Capstone");
        project.tree.add_module("embeddings").unwrap();
        let submodule = project
            .tree
            .modules
            .get_mut(&disk::EntryName("embeddings".to_string()))
            .unwrap();
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        submodule
            .add_requirement("definition", requirement)
            .unwrap();

        let project = validated(project);
        // The submodule's requirement has no tests, so it's not met.
        assert!(!project.all_requirements_met_in_subtree(&[]));
    }

    #[test]
    fn all_requirements_met_in_subtree_is_false_for_an_unknown_module() {
        let project = validated(create_project("Capstone"));
        assert!(
            !project.all_requirements_met_in_subtree(&[disk::EntryName("nonexistent".to_string())])
        );
    }

    #[test]
    fn into_draft_and_draft_hand_back_the_underlying_project() {
        let project = validated(project_with_current_requirement_and_test());
        assert_eq!(project.draft().definition.name, "Capstone");
        let draft = project.into_draft();
        assert_eq!(draft.definition.name, "Capstone");
    }

    #[test]
    fn save_writes_a_loadable_project() {
        let project = validated(project_with_current_requirement_and_test());

        let dir = std::env::temp_dir().join(format!(
            "logical-validated-save-{}-{}",
            std::process::id(),
            line!()
        ));
        project.save(&syscalls::StdFilesystem, &dir).unwrap();

        let reloaded = disk::load_project(
            &syscalls::StdFilesystem,
            &crate::test_support::FixedGit,
            &dir,
        )
        .unwrap();
        assert_eq!(reloaded.definition.name, "Capstone");
        assert_eq!(reloaded.tree.requirements.len(), 1);
        assert_eq!(reloaded.tree.tests.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
