//! Center-pane forms — one type per entity kind, per README's "Center
//! pane: distinct forms per kind." Each form serves **both** creating a
//! new entry and editing an existing one, distinguished by
//! `editing_target`: `None` means "create" (`build_command` sends an
//! `Add*` `Command`, `module` names where); `Some(target)` means "editing
//! `target`" (`build_command` sends the matching `Update*` `Command`
//! instead, ignoring `module` — an existing entry's location doesn't
//! change through this form). Each form owns its own
//! `pending_request`/`error` so a failure reports inline and leaves the
//! form open to fix and retry, rather than silently discarding what the
//! user typed.

use std::path::PathBuf;

use gui_core::{
    Command, DependencyReferenceKind, EntryName, LocalGitReference, LogicalPath, ReferencePath, RemoteGitReference,
    RequestId, RequirementDraft, ResultDraft, ResultKindV1, TestDraft,
};

fn non_empty(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// A requirement's `dependency`/`dependencies` field, one entry per
/// dependency, in the plain-`String`-fields shape `egui`'s text widgets
/// need — converted to/from `gui_core::DependencyReferenceKind` at the
/// form's edges (`from_core`/`to_core`), never held as the wire type
/// itself. Unlike `attachments`, editing this list is plain local form
/// state, submitted whole via `Command::UpdateRequirement`/`AddRequirement`
/// on Save — no per-item round trip, since a dependency isn't a file to
/// copy into place, just a reference (see `EntryDetail::Requirement`'s
/// own doc comment on `dependencies`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyDraft {
    /// `DependencyReferenceKind::RequirementReferenceV1` — a stage
    /// elsewhere in this same project.
    LocalRequirement { path: String, commit: String },
    /// `DependencyReferenceKind::RemoteReferenceV1` — a stage in a
    /// different git repository. `path` empty means "no path" (the
    /// remote repository's own root), same as the on-disk `Option`
    /// collapsing to an empty field here — see `RemoteGitReference`'s own
    /// doc comment on what an absent path means.
    Remote { url: String, path: String, commit: String },
    /// The bare `Submodules` variant — no fields, satisfied when every
    /// requirement in the current module's entire submodule subtree is
    /// met (see `logical`'s README on "Validation questions — answered").
    Submodules,
}

impl DependencyDraft {
    pub fn from_core(kind: DependencyReferenceKind) -> DependencyDraft {
        match kind {
            DependencyReferenceKind::RequirementReferenceV1(local) => DependencyDraft::LocalRequirement {
                path: local.path.0,
                commit: local.commit,
            },
            DependencyReferenceKind::RemoteReferenceV1(remote) => DependencyDraft::Remote {
                url: remote.url,
                path: remote.path.map(|p| p.0).unwrap_or_default(),
                commit: remote.commit,
            },
            DependencyReferenceKind::Submodules => DependencyDraft::Submodules,
        }
    }

    pub fn to_core(&self) -> DependencyReferenceKind {
        match self {
            DependencyDraft::LocalRequirement { path, commit } => {
                DependencyReferenceKind::RequirementReferenceV1(LocalGitReference {
                    path: ReferencePath(path.clone()),
                    commit: commit.clone(),
                })
            }
            DependencyDraft::Remote { url, path, commit } => {
                DependencyReferenceKind::RemoteReferenceV1(RemoteGitReference {
                    url: url.clone(),
                    path: if path.trim().is_empty() { None } else { Some(ReferencePath(path.clone())) },
                    commit: commit.clone(),
                })
            }
            DependencyDraft::Submodules => DependencyReferenceKind::Submodules,
        }
    }
}

impl Default for DependencyDraft {
    /// The "Add dependency" composer's starting shape — a local
    /// requirement reference is the common case, so that's the default
    /// rather than an arbitrary pick.
    fn default() -> DependencyDraft {
        DependencyDraft::LocalRequirement {
            path: String::new(),
            commit: String::new(),
        }
    }
}

impl std::fmt::Display for DependencyDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencyDraft::LocalRequirement { path, commit } => write!(f, "{path} @ {commit}"),
            DependencyDraft::Remote { url, path, commit } if path.is_empty() => write!(f, "{url} @ {commit}"),
            DependencyDraft::Remote { url, path, commit } => write!(f, "{url}{path} @ {commit}"),
            DependencyDraft::Submodules => write!(f, "Submodules (all submodules must be met)"),
        }
    }
}

#[derive(Debug, Default)]
pub struct RequirementFormState {
    pub name: String,
    pub title: String,
    pub requirement_text: String,
    pub requirement_guidance: String,
    pub test_guidance: String,
    pub editing_target: Option<LogicalPath>,
    /// `true` for the read-only viewer, `false` for the editable form —
    /// only meaningful alongside `editing_target: Some(_)` (a
    /// creation-mode form is always editable; see `GuiApp::apply_entry_detail`
    /// for where an existing entry's form starts `true`, and
    /// `GuiApp::editor_edit_clicked`/`editor_cancel_clicked` for how it
    /// flips between the two).
    pub read_only: bool,
    /// `true` once any editable field/dependency has actually changed
    /// since the form was last (re)loaded or successfully saved — never
    /// set while `read_only` (nothing editable renders then). Drives the
    /// "you have unsaved changes" prompt any navigation away from this
    /// form triggers — see `GuiApp::editor_has_unsaved_edits` and
    /// `PendingNavigation`. Reset `false` in `apply_entry_detail` (a
    /// freshly (re)loaded form starts pristine) and on a successful
    /// `Update*` (`apply_update_result` — the displayed values now match
    /// what's saved, so leaving no longer needs confirming); left `true`
    /// on a *failed* save so the warning still applies to what's still
    /// sitting unsaved in the form. Deliberately not attachments — those
    /// submit their own `Command` immediately on Add/Remove rather than
    /// waiting for this form's own Save, so they're already covered by
    /// `GuiApp::dirty` at the project level, not this.
    pub edited: bool,
    pub pending_request: Option<RequestId>,
    pub error: Option<String>,
    /// This requirement's `dependency`/`dependencies` field — see
    /// `DependencyDraft`'s own doc comment.
    pub dependencies: Vec<DependencyDraft>,
    /// The "Add dependency" composer's in-progress entry — pushed onto
    /// `dependencies` and reset to `DependencyDraft::default()` when
    /// "Add dependency" is clicked.
    pub new_dependency: DependencyDraft,
    /// This requirement's own local attachment pool — empty for a
    /// creation-mode form (there's no entry yet to attach anything to;
    /// `Command::AddRequirementAttachment` requires one to already exist),
    /// populated from `EntryDetail::Requirement` when editing. See
    /// `GuiApp::apply_local_pool_change` for how add/remove keep this in
    /// sync without a round-trip through `GetEntryDetail` (which would
    /// discard any of this form's own unsaved field edits).
    pub attachments: Vec<PathBuf>,
    pub new_attachment_path: String,
    pub local_pool_error: Option<String>,
}

impl RequirementFormState {
    pub fn build_command(&self, module: Vec<EntryName>, request: RequestId) -> Command {
        let mut requirement = RequirementDraft::new(self.title.clone());
        requirement.requirement_text = self.requirement_text.clone();
        requirement.requirement_guidance = non_empty(&self.requirement_guidance);
        requirement.test_guidance = non_empty(&self.test_guidance);
        requirement.dependencies = self.dependencies.iter().map(DependencyDraft::to_core).collect();
        match &self.editing_target {
            Some(target) => Command::UpdateRequirement {
                target: target.clone(),
                requirement: Box::new(requirement),
                request,
            },
            None => Command::AddRequirement {
                module,
                name: EntryName(self.name.clone()),
                requirement: Box::new(requirement),
                request,
            },
        }
    }
}

#[derive(Debug)]
pub struct TestFormState {
    pub name: String,
    pub title: String,
    pub result_kind: ResultKindV1,
    pub editing_target: Option<LogicalPath>,
    /// See `RequirementFormState::read_only`'s doc comment — same idea.
    pub read_only: bool,
    /// See `RequirementFormState::edited`'s doc comment — same idea.
    pub edited: bool,
    pub pending_request: Option<RequestId>,
    pub error: Option<String>,
    /// See `RequirementFormState::attachments`'s doc comment — same idea.
    pub attachments: Vec<PathBuf>,
    pub new_attachment_path: String,
    /// A test's local *template* pool — distinct from `attachments`, see
    /// `Command::AddTestTemplateFile`.
    pub template_files: Vec<PathBuf>,
    pub new_template_path: String,
    pub local_pool_error: Option<String>,
}

impl Default for TestFormState {
    fn default() -> TestFormState {
        TestFormState {
            name: String::new(),
            title: String::new(),
            result_kind: ResultKindV1::FreeForm,
            editing_target: None,
            read_only: false,
            edited: false,
            pending_request: None,
            error: None,
            attachments: Vec::new(),
            new_attachment_path: String::new(),
            template_files: Vec::new(),
            new_template_path: String::new(),
            local_pool_error: None,
        }
    }
}

impl TestFormState {
    pub fn build_command(&self, module: Vec<EntryName>, request: RequestId) -> Command {
        let test = TestDraft::new(self.title.clone(), self.result_kind.clone());
        match &self.editing_target {
            Some(target) => Command::UpdateTest {
                target: target.clone(),
                test: Box::new(test),
                request,
            },
            None => Command::AddTest {
                module,
                name: EntryName(self.name.clone()),
                test: Box::new(test),
                request,
            },
        }
    }
}

/// `requirement_path`/`test_path`/their commits are typed as plain text
/// here — there's no reference-picker UI yet (that needs the tree to
/// support "pick an entry" selection mode, not just click-to-view). A
/// user has to know the reference path/commit to type in by hand, same as
/// hand-authoring the RON directly would require.
#[derive(Debug, Default)]
pub struct ResultFormState {
    pub name: String,
    pub title: String,
    pub requirement_path: String,
    pub requirement_commit: String,
    pub test_path: String,
    pub test_commit: String,
    pub editing_target: Option<LogicalPath>,
    /// See `RequirementFormState::read_only`'s doc comment — same idea.
    pub read_only: bool,
    /// See `RequirementFormState::edited`'s doc comment — same idea.
    pub edited: bool,
    pub pending_request: Option<RequestId>,
    pub error: Option<String>,
    /// See `RequirementFormState::attachments`'s doc comment — same idea.
    pub attachments: Vec<PathBuf>,
    pub new_attachment_path: String,
    pub local_pool_error: Option<String>,
}

impl ResultFormState {
    pub fn build_command(&self, module: Vec<EntryName>, request: RequestId) -> Command {
        let result = ResultDraft::new(
            self.title.clone(),
            gui_core::ReferencePath(self.requirement_path.clone()),
            self.requirement_commit.clone(),
            gui_core::ReferencePath(self.test_path.clone()),
            self.test_commit.clone(),
        );
        match &self.editing_target {
            Some(target) => Command::UpdateResult {
                target: target.clone(),
                result: Box::new(result),
                request,
            },
            None => Command::AddResult {
                module,
                name: EntryName(self.name.clone()),
                result: Box::new(result),
                request,
            },
        }
    }
}

/// Modules have no editable content of their own beyond their name/
/// children (which the tree already shows), and renaming isn't supported
/// — so, unlike the other three, this form is creation-only. No
/// `editing_target`.
#[derive(Debug, Default)]
pub struct ModuleFormState {
    pub name: String,
    pub pending_request: Option<RequestId>,
    pub error: Option<String>,
}

impl ModuleFormState {
    pub fn build_command(&self, module: Vec<EntryName>, request: RequestId) -> Command {
        Command::AddModule {
            module,
            name: EntryName(self.name.clone()),
            request,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_local_requirement_dependency_round_trips_through_core() {
        let core = DependencyReferenceKind::RequirementReferenceV1(LocalGitReference {
            path: ReferencePath("/requirements/discovery".to_string()),
            commit: "abc123".to_string(),
        });
        let draft = DependencyDraft::from_core(core);
        assert_eq!(
            draft,
            DependencyDraft::LocalRequirement {
                path: "/requirements/discovery".to_string(),
                commit: "abc123".to_string(),
            }
        );

        let DependencyReferenceKind::RequirementReferenceV1(local) = draft.to_core() else {
            panic!("expected RequirementReferenceV1");
        };
        assert_eq!(local.path.0, "/requirements/discovery");
        assert_eq!(local.commit, "abc123");
    }

    #[test]
    fn a_remote_dependency_with_a_path_round_trips_through_core() {
        let core = DependencyReferenceKind::RemoteReferenceV1(RemoteGitReference {
            url: "https://example.com/repo.git".to_string(),
            path: Some(ReferencePath("/requirements/upstream".to_string())),
            commit: "def456".to_string(),
        });
        let draft = DependencyDraft::from_core(core);
        assert_eq!(
            draft,
            DependencyDraft::Remote {
                url: "https://example.com/repo.git".to_string(),
                path: "/requirements/upstream".to_string(),
                commit: "def456".to_string(),
            }
        );

        let DependencyReferenceKind::RemoteReferenceV1(remote) = draft.to_core() else {
            panic!("expected RemoteReferenceV1");
        };
        assert_eq!(remote.path.map(|p| p.0), Some("/requirements/upstream".to_string()));
    }

    #[test]
    fn a_remote_dependency_with_no_path_collapses_to_an_empty_field_and_back_to_none() {
        let core = DependencyReferenceKind::RemoteReferenceV1(RemoteGitReference {
            url: "https://example.com/repo.git".to_string(),
            path: None,
            commit: "def456".to_string(),
        });
        let draft = DependencyDraft::from_core(core);
        let DependencyDraft::Remote { path, .. } = &draft else {
            panic!("expected Remote");
        };
        assert_eq!(path, "");

        let DependencyReferenceKind::RemoteReferenceV1(remote) = draft.to_core() else {
            panic!("expected RemoteReferenceV1");
        };
        assert_eq!(remote.path, None);
    }

    #[test]
    fn a_remote_dependencys_blank_path_field_also_collapses_to_none() {
        // Not just an untouched default — a path field the user typed
        // into and then cleared back out must behave the same as one
        // never touched at all.
        let draft = DependencyDraft::Remote {
            url: "https://example.com/repo.git".to_string(),
            path: "   ".to_string(),
            commit: "def456".to_string(),
        };
        let DependencyReferenceKind::RemoteReferenceV1(remote) = draft.to_core() else {
            panic!("expected RemoteReferenceV1");
        };
        assert_eq!(remote.path, None);
    }

    #[test]
    fn submodules_round_trips_through_core() {
        let draft = DependencyDraft::from_core(DependencyReferenceKind::Submodules);
        assert_eq!(draft, DependencyDraft::Submodules);
        assert!(matches!(draft.to_core(), DependencyReferenceKind::Submodules));
    }

    #[test]
    fn the_default_dependency_draft_is_an_empty_local_requirement() {
        assert_eq!(
            DependencyDraft::default(),
            DependencyDraft::LocalRequirement {
                path: String::new(),
                commit: String::new(),
            }
        );
    }

    #[test]
    fn requirement_form_build_command_carries_dependencies_through() {
        let mut form = RequirementFormState {
            title: "Title".to_string(),
            ..Default::default()
        };
        form.dependencies.push(DependencyDraft::LocalRequirement {
            path: "/requirements/discovery".to_string(),
            commit: "abc123".to_string(),
        });
        form.dependencies.push(DependencyDraft::Submodules);

        let Command::AddRequirement { requirement, .. } = form.build_command(Vec::new(), 1) else {
            panic!("expected AddRequirement");
        };
        assert_eq!(requirement.dependencies.len(), 2);
        assert!(matches!(
            &requirement.dependencies[0],
            DependencyReferenceKind::RequirementReferenceV1(local) if local.path.0 == "/requirements/discovery"
        ));
        assert!(matches!(&requirement.dependencies[1], DependencyReferenceKind::Submodules));
    }
}
