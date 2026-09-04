use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use disk::{
    AttachmentReferenceKind, DependencyReferenceKind, EntryName, ReferencePath, ResultKindV1,
    TemplateReferenceKind, TestReferenceKind,
};
use syscalls::RemoteGit;

use super::cycle::find_cycles;
use super::error::{PoolKind, UnresolvedTarget, ValidationError};
use crate::LogicalPath;
use crate::draft::{ModuleDraft, ProjectDraft, RequirementDraft, ResultDraft, TestDraft};
use crate::lookup::{get_module, get_requirement, get_test};
use crate::path::parse_reference_path;
use crate::validated::ValidatedProject;

/// Validates a whole `ProjectDraft` — see `crates/logical/README.md`'s
/// "What validation checks" and the sections that follow it. Collects
/// every violation found rather than stopping at the first (`validate()`
/// returns `Vec<ValidationError>` on failure, not a single early-exit
/// error — deliberately different from `disk`'s per-function style, see
/// the README's "Validation collects every error" note).
///
/// `remote_git` resolves `RemoteReferenceV1` dependencies eagerly, over
/// the network — see "Validation questions — answered" #2.
pub fn validate(
    draft: ProjectDraft,
    remote_git: &dyn RemoteGit,
) -> Result<ValidatedProject, Vec<ValidationError>> {
    let mut ctx = Context {
        root: &draft.tree,
        unresolved: BTreeMap::new(),
        other_errors: Vec::new(),
        dependency_edges: BTreeMap::new(),
    };

    ctx.walk_module(&draft.tree, &[], remote_git);

    let mut errors: Vec<ValidationError> = ctx
        .unresolved
        .into_iter()
        .map(
            |(target, referenced_by)| ValidationError::UnresolvedReference {
                target,
                referenced_by,
            },
        )
        .collect();
    errors.extend(ctx.other_errors);
    errors.extend(
        find_cycles(&ctx.dependency_edges)
            .into_iter()
            .map(|cycle| ValidationError::DependencyCycle { cycle }),
    );

    if errors.is_empty() {
        Ok(ValidatedProject::new(draft))
    } else {
        Err(errors)
    }
}

struct Context<'a> {
    root: &'a ModuleDraft,
    unresolved: BTreeMap<UnresolvedTarget, Vec<LogicalPath>>,
    other_errors: Vec<ValidationError>,
    dependency_edges: BTreeMap<LogicalPath, Vec<LogicalPath>>,
}

impl<'a> Context<'a> {
    fn record_unresolved(&mut self, target: UnresolvedTarget, referenced_by: LogicalPath) {
        self.unresolved
            .entry(target)
            .or_default()
            .push(referenced_by);
    }

    fn walk_module(
        &mut self,
        module: &'a ModuleDraft,
        prefix: &[EntryName],
        remote_git: &dyn RemoteGit,
    ) {
        for (name, requirement) in &module.requirements {
            let path = LogicalPath {
                modules: prefix.to_vec(),
                name: name.clone(),
            };
            self.dependency_edges.entry(path.clone()).or_default();
            self.check_requirement(&path, requirement, remote_git);
        }
        for (name, test) in &module.tests {
            let path = LogicalPath {
                modules: prefix.to_vec(),
                name: name.clone(),
            };
            self.check_test(&path, test);
        }
        for (name, result) in &module.results {
            let path = LogicalPath {
                modules: prefix.to_vec(),
                name: name.clone(),
            };
            self.check_result(&path, result);
        }
        for (name, submodule) in &module.modules {
            let mut child_prefix = prefix.to_vec();
            child_prefix.push(name.clone());
            self.walk_module(submodule, &child_prefix, remote_git);
        }
    }

    fn check_requirement(
        &mut self,
        path: &LogicalPath,
        requirement: &'a RequirementDraft,
        remote_git: &dyn RemoteGit,
    ) {
        for test_ref in &requirement.tests {
            let TestReferenceKind::TestReferenceV1(local) = test_ref;
            self.resolve_test(&local.path, &path.modules, path);
        }

        for dependency in &requirement.dependencies {
            match dependency {
                DependencyReferenceKind::RequirementReferenceV1(local) => {
                    if let Some(target) = self.resolve_requirement(&local.path, &path.modules, path)
                    {
                        self.dependency_edges
                            .entry(path.clone())
                            .or_default()
                            .push(target);
                    }
                }
                DependencyReferenceKind::RemoteReferenceV1(remote) => {
                    let remote_path = remote.path.as_ref().map(|p| Path::new(p.0.as_str()));
                    if let Err(source) = remote_git.commit_for_remote(&remote.url, remote_path) {
                        self.other_errors
                            .push(ValidationError::RemoteResolutionFailed {
                                referenced_by: path.clone(),
                                url: remote.url.clone(),
                                message: source.to_string(),
                            });
                    }
                }
                DependencyReferenceKind::Submodules => {
                    // Satisfaction is query-time (see "Validation questions
                    // — answered" #3) — nothing to resolve here.
                }
            }
        }

        for attachment_ref in &requirement.attachment_refs {
            self.resolve_attachment_ref(attachment_ref, path, &requirement.attachments);
        }

        self.check_local_pool(
            path,
            &requirement.attachments,
            local_attachment_paths(&requirement.attachment_refs),
            PoolKind::Attachments,
        );
    }

    fn check_test(&mut self, path: &LogicalPath, test: &'a TestDraft) {
        for attachment_ref in &test.attachment_refs {
            self.resolve_attachment_ref(attachment_ref, path, &test.attachments);
        }
        self.check_local_pool(
            path,
            &test.attachments,
            local_attachment_paths(&test.attachment_refs),
            PoolKind::Attachments,
        );

        for template_ref in &test.template_refs {
            self.resolve_template_ref(template_ref, path, &test.template);
        }
        self.check_local_pool(
            path,
            &test.template,
            local_template_paths(&test.template_refs),
            PoolKind::Template,
        );
    }

    fn check_result(&mut self, path: &LogicalPath, result: &'a ResultDraft) {
        for attachment_ref in &result.attachment_refs {
            self.resolve_attachment_ref(attachment_ref, path, &result.attachments);
        }
        self.check_local_pool(
            path,
            &result.attachments,
            local_attachment_paths(&result.attachment_refs),
            PoolKind::Attachments,
        );

        if let Some(target_path) = self.resolve_test(&result.test_path, &path.modules, path) {
            // `resolve_test` only ever returns `Some` after confirming
            // `get_test` finds something at that path, so this can't fail.
            let test = get_test(self.root, &target_path)
                .expect("resolve_test already confirmed this test exists");
            if matches!(test.result_kind, ResultKindV1::Template) {
                let template_names: BTreeSet<String> = test
                    .template_refs
                    .iter()
                    .filter_map(|reference| file_name(template_ref_path(reference)))
                    .collect();
                let result_names: BTreeSet<String> = result
                    .attachments
                    .iter()
                    .filter_map(|p| file_name(p))
                    .collect();
                let missing: Vec<String> =
                    template_names.difference(&result_names).cloned().collect();
                if !missing.is_empty() {
                    self.other_errors
                        .push(ValidationError::TemplateCoverageMismatch {
                            test: target_path,
                            result: path.clone(),
                            missing_file_names: missing,
                        });
                }
            }
        }
    }

    fn check_local_pool(
        &mut self,
        entity: &LogicalPath,
        physical: &BTreeSet<PathBuf>,
        declared_local: BTreeSet<PathBuf>,
        pool: PoolKind,
    ) {
        let undeclared: Vec<PathBuf> = physical.difference(&declared_local).cloned().collect();
        if !undeclared.is_empty() {
            self.other_errors.push(ValidationError::LocalPoolMismatch {
                entity: entity.clone(),
                pool,
                undeclared,
            });
        }
    }

    fn resolve_requirement(
        &mut self,
        raw: &ReferencePath,
        current_module: &[EntryName],
        referenced_by: &LogicalPath,
    ) -> Option<LogicalPath> {
        match parse_reference_path(raw, current_module, "requirements") {
            Ok(target) => {
                if get_requirement(self.root, &target).is_some() {
                    Some(target)
                } else {
                    self.record_unresolved(
                        UnresolvedTarget::Requirement(target),
                        referenced_by.clone(),
                    );
                    None
                }
            }
            Err(_) => {
                self.record_unresolved(
                    UnresolvedTarget::MalformedReference { raw: raw.0.clone() },
                    referenced_by.clone(),
                );
                None
            }
        }
    }

    fn resolve_test(
        &mut self,
        raw: &ReferencePath,
        current_module: &[EntryName],
        referenced_by: &LogicalPath,
    ) -> Option<LogicalPath> {
        match parse_reference_path(raw, current_module, "tests") {
            Ok(target) => {
                if get_test(self.root, &target).is_some() {
                    Some(target)
                } else {
                    self.record_unresolved(UnresolvedTarget::Test(target), referenced_by.clone());
                    None
                }
            }
            Err(_) => {
                self.record_unresolved(
                    UnresolvedTarget::MalformedReference { raw: raw.0.clone() },
                    referenced_by.clone(),
                );
                None
            }
        }
    }

    fn resolve_attachment_ref(
        &mut self,
        reference: &AttachmentReferenceKind,
        entity: &LogicalPath,
        local_pool: &BTreeSet<PathBuf>,
    ) {
        match reference {
            AttachmentReferenceKind::LocalAttachmentReferenceV1 { path, .. } => {
                if !local_pool.contains(path) {
                    self.record_unresolved(
                        UnresolvedTarget::LocalAttachment {
                            entity: entity.clone(),
                            path: path.clone(),
                        },
                        entity.clone(),
                    );
                }
            }
            AttachmentReferenceKind::ModuleAttachmentReferenceV1 { path, .. } => {
                let module = get_module(self.root, &entity.modules);
                let found = module.is_some_and(|m| m.attachments.contains(path));
                if !found {
                    self.record_unresolved(
                        UnresolvedTarget::ModuleAttachment {
                            module: entity.modules.clone(),
                            path: path.clone(),
                        },
                        entity.clone(),
                    );
                }
            }
        }
    }

    fn resolve_template_ref(
        &mut self,
        reference: &TemplateReferenceKind,
        entity: &LogicalPath,
        local_pool: &BTreeSet<PathBuf>,
    ) {
        match reference {
            TemplateReferenceKind::LocalTemplateReferenceV1 { path, .. } => {
                if !local_pool.contains(path) {
                    self.record_unresolved(
                        UnresolvedTarget::LocalTemplate {
                            entity: entity.clone(),
                            path: path.clone(),
                        },
                        entity.clone(),
                    );
                }
            }
            TemplateReferenceKind::ModuleTemplateReferenceV1 { path, .. } => {
                let module = get_module(self.root, &entity.modules);
                let found = module.is_some_and(|m| m.templates.contains(path));
                if !found {
                    self.record_unresolved(
                        UnresolvedTarget::ModuleTemplate {
                            module: entity.modules.clone(),
                            path: path.clone(),
                        },
                        entity.clone(),
                    );
                }
            }
        }
    }
}

fn local_attachment_paths(refs: &[AttachmentReferenceKind]) -> BTreeSet<PathBuf> {
    refs.iter()
        .filter_map(|reference| match reference {
            AttachmentReferenceKind::LocalAttachmentReferenceV1 { path, .. } => Some(path.clone()),
            AttachmentReferenceKind::ModuleAttachmentReferenceV1 { .. } => None,
        })
        .collect()
}

fn local_template_paths(refs: &[TemplateReferenceKind]) -> BTreeSet<PathBuf> {
    refs.iter()
        .filter_map(|reference| match reference {
            TemplateReferenceKind::LocalTemplateReferenceV1 { path, .. } => Some(path.clone()),
            TemplateReferenceKind::ModuleTemplateReferenceV1 { .. } => None,
        })
        .collect()
}

fn template_ref_path(reference: &TemplateReferenceKind) -> &PathBuf {
    match reference {
        TemplateReferenceKind::LocalTemplateReferenceV1 { path, .. } => path,
        TemplateReferenceKind::ModuleTemplateReferenceV1 { path, .. } => path,
    }
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::draft::create_project;
    use crate::test_support::{FixedGit, FixedRemoteGit};
    use disk::{
        AttachmentReferenceKind, DependencyReferenceKind, LocalGitReference, RemoteGitReference,
        StatusV1, TestReferenceKind,
    };
    use syscalls::{CommitForRemoteError, StdFilesystem};

    /// A minimal, fully-resolving project: requirement "definition" ->
    /// test "generic_test" (both at commit "c1"), no dependencies, no
    /// attachments/templates.
    fn minimal_project() -> ProjectDraft {
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

    #[test]
    fn a_minimal_project_validates_with_no_errors() {
        let project = minimal_project();
        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    #[test]
    fn loads_and_validates_the_whole_test_project_cleanly() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project");
        let on_disk = disk::load_project(&StdFilesystem, &FixedGit, &dir).unwrap();
        let draft = crate::convert::import_project(on_disk);

        let result = validate(draft, &FixedRemoteGit);
        assert!(
            result.is_ok(),
            "sample project should validate cleanly: {:?}",
            result.err()
        );
    }

    #[test]
    fn reports_an_unresolved_test_reference() {
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

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::Test(_),
                ..
            }
        ));
    }

    #[test]
    fn reports_a_malformed_dependency_reference_path() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::RequirementReferenceV1(
                LocalGitReference {
                    path: ReferencePath("requirements".to_string()),
                    commit: "c1".to_string(),
                },
            ));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::MalformedReference { .. },
                ..
            }
        ));
    }

    #[test]
    fn reports_a_malformed_reference_path() {
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

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::MalformedReference { .. },
                ..
            }
        ));
    }

    #[test]
    fn groups_multiple_referencers_of_the_same_missing_dependency() {
        let mut project = create_project("Capstone");
        for name in ["a", "b"] {
            let mut requirement = RequirementDraft::new(name);
            requirement.commit = Some("c1".to_string());
            requirement
                .dependencies
                .push(DependencyReferenceKind::RequirementReferenceV1(
                    LocalGitReference {
                        path: ReferencePath("requirements/nonexistent".to_string()),
                        commit: "c1".to_string(),
                    },
                ));
            project.tree.add_requirement(name, requirement).unwrap();
        }

        // Two requirements reference the same missing dependency — grouped
        // into one error (not two), which `errors.len() == 1` alone proves:
        // ungrouped, this would be two separate `UnresolvedReference`s.
        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference { .. }
        ));
        assert!(errors[0].to_string().contains("nonexistent"));
    }

    #[test]
    fn reports_a_dependency_cycle() {
        let mut project = create_project("Capstone");
        for (name, target) in [("a", "b"), ("b", "a")] {
            let mut requirement = RequirementDraft::new(name);
            requirement.commit = Some("c1".to_string());
            requirement
                .dependencies
                .push(DependencyReferenceKind::RequirementReferenceV1(
                    LocalGitReference {
                        path: ReferencePath(format!("requirements/{target}")),
                        commit: "c1".to_string(),
                    },
                ));
            project.tree.add_requirement(name, requirement).unwrap();
        }

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ValidationError::DependencyCycle { .. }))
        );
    }

    #[test]
    fn a_submodules_dependency_never_needs_resolution() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::Submodules);
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    #[test]
    fn a_resolvable_remote_dependency_is_accepted() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::RemoteReferenceV1(
                RemoteGitReference {
                    url: "https://example.com/repo.git".to_string(),
                    path: None,
                    commit: "c1".to_string(),
                },
            ));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    #[test]
    fn a_resolvable_remote_dependency_with_a_path_is_accepted() {
        // Covers the `remote.path.as_ref().map(...)` closure specifically —
        // `a_resolvable_remote_dependency_is_accepted` above only exercises
        // `path: None`.
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::RemoteReferenceV1(
                RemoteGitReference {
                    url: "https://example.com/repo.git".to_string(),
                    path: Some(ReferencePath("some/path".to_string())),
                    commit: "c1".to_string(),
                },
            ));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    struct FailingRemoteGit;
    impl RemoteGit for FailingRemoteGit {
        fn commit_for_remote(
            &self,
            url: &str,
            _path: Option<&Path>,
        ) -> Result<String, CommitForRemoteError> {
            Err(CommitForRemoteError::Empty {
                url: url.to_string(),
            })
        }
    }

    #[test]
    fn reports_a_failing_remote_dependency() {
        let mut project = create_project("Capstone");
        let mut requirement = RequirementDraft::new("Definition");
        requirement.commit = Some("c1".to_string());
        requirement
            .dependencies
            .push(DependencyReferenceKind::RemoteReferenceV1(
                RemoteGitReference {
                    url: "https://example.com/repo.git".to_string(),
                    path: None,
                    commit: "c1".to_string(),
                },
            ));
        project
            .tree
            .add_requirement("definition", requirement)
            .unwrap();

        let errors = validate(project, &FailingRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::RemoteResolutionFailed { .. }
        ));
    }

    #[test]
    fn reports_an_unresolved_local_attachment_reference() {
        let mut project = minimal_project();
        let requirement = project
            .tree
            .requirements
            .get_mut(&EntryName("definition".to_string()))
            .unwrap();
        requirement
            .attachment_refs
            .push(AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                name: EntryName("missing".to_string()),
                path: PathBuf::from("missing.txt"),
            });

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::LocalAttachment { .. },
                ..
            }
        ));
    }

    #[test]
    fn reports_an_unresolved_module_attachment_reference() {
        let mut project = minimal_project();
        let requirement = project
            .tree
            .requirements
            .get_mut(&EntryName("definition".to_string()))
            .unwrap();
        requirement
            .attachment_refs
            .push(AttachmentReferenceKind::ModuleAttachmentReferenceV1 {
                name: EntryName("missing".to_string()),
                path: PathBuf::from("missing.txt"),
            });

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::ModuleAttachment { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_resolvable_local_attachment_reference_is_accepted() {
        let mut project = minimal_project();
        let requirement = project
            .tree
            .requirements
            .get_mut(&EntryName("definition".to_string()))
            .unwrap();
        requirement.add_attachment(Path::new("notes.txt")).unwrap();
        requirement
            .attachment_refs
            .push(AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                name: EntryName("notes".to_string()),
                path: PathBuf::from("notes.txt"),
            });

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    #[test]
    fn a_resolvable_module_attachment_reference_is_accepted() {
        let mut project = minimal_project();
        project
            .tree
            .add_attachment(Path::new("shared.txt"))
            .unwrap();
        let requirement = project
            .tree
            .requirements
            .get_mut(&EntryName("definition".to_string()))
            .unwrap();
        requirement
            .attachment_refs
            .push(AttachmentReferenceKind::ModuleAttachmentReferenceV1 {
                name: EntryName("shared".to_string()),
                path: PathBuf::from("shared.txt"),
            });

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    #[test]
    fn reports_an_undeclared_local_attachment_file() {
        let mut project = minimal_project();
        let requirement = project
            .tree
            .requirements
            .get_mut(&EntryName("definition".to_string()))
            .unwrap();
        requirement
            .add_attachment(Path::new("undeclared.txt"))
            .unwrap();

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::LocalPoolMismatch {
                pool: PoolKind::Attachments,
                ..
            }
        ));
    }

    #[test]
    fn reports_an_unresolved_local_template_reference() {
        let mut project = minimal_project();
        let test = project
            .tree
            .tests
            .get_mut(&EntryName("generic_test".to_string()))
            .unwrap();
        test.template_refs
            .push(TemplateReferenceKind::LocalTemplateReferenceV1 {
                name: EntryName("missing".to_string()),
                path: PathBuf::from("missing.typ"),
            });

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::LocalTemplate { .. },
                ..
            }
        ));
    }

    #[test]
    fn reports_an_unresolved_module_template_reference() {
        let mut project = minimal_project();
        let test = project
            .tree
            .tests
            .get_mut(&EntryName("generic_test".to_string()))
            .unwrap();
        test.template_refs
            .push(TemplateReferenceKind::ModuleTemplateReferenceV1 {
                name: EntryName("missing".to_string()),
                path: PathBuf::from("missing.typ"),
            });

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::ModuleTemplate { .. },
                ..
            }
        ));
    }

    #[test]
    fn reports_an_undeclared_local_template_file() {
        let mut project = minimal_project();
        let test = project
            .tree
            .tests
            .get_mut(&EntryName("generic_test".to_string()))
            .unwrap();
        test.add_template_file(Path::new("undeclared.typ")).unwrap();

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(matches!(
            &errors[0],
            ValidationError::LocalPoolMismatch {
                pool: PoolKind::Template,
                ..
            }
        ));
    }

    fn project_with_template_test() -> ProjectDraft {
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

        project.tree.add_template(Path::new("shared.typ")).unwrap();

        let mut test = TestDraft::new("Generic Test", ResultKindV1::Template);
        test.commit = Some("t1".to_string());
        test.add_template_file(Path::new("spec.typ")).unwrap();
        test.template_refs
            .push(TemplateReferenceKind::LocalTemplateReferenceV1 {
                name: EntryName("spec".to_string()),
                path: PathBuf::from("spec.typ"),
            });
        test.template_refs
            .push(TemplateReferenceKind::ModuleTemplateReferenceV1 {
                name: EntryName("shared".to_string()),
                path: PathBuf::from("shared.typ"),
            });
        project.tree.add_test("generic_test", test).unwrap();

        project
    }

    #[test]
    fn reports_a_template_coverage_mismatch() {
        let mut project = project_with_template_test();
        let mut result = ResultDraft::new(
            "Definition",
            ReferencePath("requirements/definition".to_string()),
            "c1",
            ReferencePath("/tests/generic_test".to_string()),
            "t1",
        );
        result.status = StatusV1::Pass;
        // Attachment file name doesn't match the test's "spec.typ" template.
        result.add_attachment(Path::new("wrong.typ")).unwrap();
        project.tree.add_result("definition", result).unwrap();

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ValidationError::TemplateCoverageMismatch { .. }))
        );
    }

    #[test]
    fn a_result_covering_every_template_file_is_accepted() {
        let mut project = project_with_template_test();
        let mut result = ResultDraft::new(
            "Definition",
            ReferencePath("requirements/definition".to_string()),
            "c1",
            ReferencePath("/tests/generic_test".to_string()),
            "t1",
        );
        result.status = StatusV1::Pass;
        result.add_attachment(Path::new("spec.typ")).unwrap();
        result
            .attachment_refs
            .push(AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                name: EntryName("spec".to_string()),
                path: PathBuf::from("spec.typ"),
            });
        // The test also declares a module-level "shared.typ" template file
        // (see `project_with_template_test`) — cover that one too.
        result.add_attachment(Path::new("shared.typ")).unwrap();
        result
            .attachment_refs
            .push(AttachmentReferenceKind::LocalAttachmentReferenceV1 {
                name: EntryName("shared".to_string()),
                path: PathBuf::from("shared.typ"),
            });
        project.tree.add_result("definition", result).unwrap();

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    #[test]
    fn a_result_referencing_a_non_template_test_skips_the_coverage_check() {
        let mut project = minimal_project();
        let result = ResultDraft::new(
            "Definition",
            ReferencePath("requirements/definition".to_string()),
            "c1",
            ReferencePath("/tests/generic_test".to_string()),
            "t1",
        );
        project.tree.add_result("definition", result).unwrap();

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }

    #[test]
    fn a_result_referencing_an_unresolvable_test_is_reported_but_skips_coverage() {
        let mut project = minimal_project();
        let result = ResultDraft::new(
            "Definition",
            ReferencePath("requirements/definition".to_string()),
            "c1",
            ReferencePath("/tests/nonexistent".to_string()),
            "t1",
        );
        project.tree.add_result("definition", result).unwrap();

        let errors = validate(project, &FixedRemoteGit).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            ValidationError::UnresolvedReference {
                target: UnresolvedTarget::Test(_),
                ..
            }
        ));
    }

    #[test]
    fn resolves_a_relative_requirement_reference_within_a_submodule() {
        let mut project = create_project("Capstone");
        project.tree.add_module("embeddings").unwrap();
        let submodule = project
            .tree
            .modules
            .get_mut(&EntryName("embeddings".to_string()))
            .unwrap();

        let mut requirement_a = RequirementDraft::new("A");
        requirement_a.commit = Some("c1".to_string());
        requirement_a
            .dependencies
            .push(DependencyReferenceKind::RequirementReferenceV1(
                LocalGitReference {
                    path: ReferencePath("requirements/b".to_string()),
                    commit: "c1".to_string(),
                },
            ));
        submodule.add_requirement("a", requirement_a).unwrap();

        let mut requirement_b = RequirementDraft::new("B");
        requirement_b.commit = Some("c1".to_string());
        submodule.add_requirement("b", requirement_b).unwrap();

        assert!(validate(project, &FixedRemoteGit).is_ok());
    }
}
