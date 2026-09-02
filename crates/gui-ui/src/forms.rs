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

use std::collections::HashMap;
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

/// Identifies one of a `RequirementFormState`'s dependency slots — an
/// existing `dependencies[index]` row, or the "Add dependency" composer's
/// own `new_dependency`. Two unrelated things key off this: an in-flight
/// `ResolveLocalCommit`/`ResolveRemoteCommit` reply (see
/// `RequirementFormState::pending_commit_fetches` and
/// `GuiApp::apply_commit_fetch_result`), and the path-picker modal's
/// `PathPickerTarget::Dependency` (see that type's own doc comment) —
/// which slot's `path` field a picked entry gets written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencySlot {
    Existing(usize),
    New,
}

/// What a dependency row's "Auto" button needs resolved, built from
/// whichever of `DependencyDraft`'s variants it belongs to — see
/// `render_dependency_fields` (where it's built, from the row's own
/// `path`/`url`/`path` fields) and `GuiApp::dependency_commit_auto_clicked`
/// (where it's turned into the matching `Command`).
#[derive(Debug, Clone)]
pub enum AutoCommitKind {
    /// `target` comes from matching the row's typed `path` field against
    /// the loaded tree's own requirement paths — see
    /// `render_dependency_fields`'s doc comment on why this can fail
    /// silently (no matching entry, or no tree loaded yet).
    Local(LogicalPath),
    Remote { url: String, path: Option<ReferencePath> },
}

#[derive(Debug)]
pub struct RequirementFormState {
    pub name: String,
    pub title: String,
    pub requirement_text: String,
    pub requirement_guidance: String,
    pub test_guidance: String,
    /// The full `RequirementDraft` this form was last (re)loaded from —
    /// `build_command` clones this and overlays only the fields above
    /// (plus `attachments`, kept in sync separately — see that field's
    /// own doc comment) rather than building a bare `RequirementDraft::new(...)`
    /// from scratch, so an `UpdateRequirement` round-trips every field
    /// this form has no UI for (`tests`, `attachment_refs`,
    /// `include_attachments_in_commit`, `commit`) instead of silently
    /// resetting them — `Command::UpdateRequirement`'s own doc comment
    /// on being a wholesale replace, not a merge, is exactly why this
    /// matters. Defaults to `RequirementDraft::new(String::new())`,
    /// matching a brand-new create-mode form (nothing to preserve yet).
    pub original: Box<RequirementDraft>,
    /// Why this requirement is (or isn't) currently `Met` — see
    /// `gui_core::RequirementMetStatus`'s own doc comment. Defaults to
    /// `Unvalidated`, matching a brand-new create-mode form (nothing to
    /// check yet — see `GuiApp::new_requirement_clicked`); populated for
    /// real by `GuiApp::apply_entry_detail`.
    pub met_status: gui_core::RequirementMetStatus,
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
    /// In-flight "Auto" commit-fetch requests, keyed by the `RequestId`
    /// each was sent under — resolved against `dependencies[index]`/
    /// `new_dependency` (per `DependencySlot`) once the matching
    /// `Outcome::ResolveLocalCommit`/`ResolveRemoteCommit` arrives. See
    /// `GuiApp::dependency_commit_auto_clicked`/`apply_commit_fetch_result`.
    pub pending_commit_fetches: HashMap<RequestId, DependencySlot>,
    /// A failed "Auto" fetch — distinct from `error` (that's this form's
    /// own Save/Create failure) since fetching a commit is a much smaller,
    /// dependency-scoped action that shouldn't read as "the whole form
    /// failed to save."
    pub commit_fetch_error: Option<String>,
}

impl Default for RequirementFormState {
    fn default() -> RequirementFormState {
        RequirementFormState {
            name: String::new(),
            title: String::new(),
            requirement_text: String::new(),
            requirement_guidance: String::new(),
            test_guidance: String::new(),
            original: Box::new(RequirementDraft::new(String::new())),
            met_status: gui_core::RequirementMetStatus::default(),
            editing_target: None,
            read_only: false,
            edited: false,
            pending_request: None,
            error: None,
            dependencies: Vec::new(),
            new_dependency: DependencyDraft::default(),
            attachments: Vec::new(),
            new_attachment_path: String::new(),
            local_pool_error: None,
            pending_commit_fetches: HashMap::new(),
            commit_fetch_error: None,
        }
    }
}

impl RequirementFormState {
    /// Starts from `self.original` — everything this form has no field
    /// for (`tests`, `attachment_refs`, `include_attachments_in_commit`,
    /// `commit`) rides along unchanged — and overlays only what's
    /// actually editable here. See `original`'s own doc comment.
    pub fn build_command(&self, module: Vec<EntryName>, request: RequestId) -> Command {
        let mut requirement = (*self.original).clone();
        requirement.title = self.title.clone();
        requirement.requirement_text = self.requirement_text.clone();
        requirement.requirement_guidance = non_empty(&self.requirement_guidance);
        requirement.test_guidance = non_empty(&self.test_guidance);
        requirement.dependencies = self.dependencies.iter().map(DependencyDraft::to_core).collect();
        // `self.original.attachments` reflects the pool as of the last
        // `GetEntryDetail`, not necessarily now — `self.attachments` is
        // the one `apply_local_pool_change` keeps live, so that's the
        // one to send.
        requirement.attachments = self.attachments.iter().cloned().collect();
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
    /// See `RequirementFormState::original`'s own doc comment — same
    /// "clone and overlay only what's editable" reasoning, preserving
    /// `attachment_refs`/`template_refs`/`include_attachments_in_commit`/
    /// `include_template_in_commit`/`commit` across an `UpdateTest`.
    pub original: Box<TestDraft>,
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
            original: Box::new(TestDraft::new(String::new(), ResultKindV1::FreeForm)),
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
        let mut test = (*self.original).clone();
        test.title = self.title.clone();
        test.result_kind = self.result_kind.clone();
        // Same "the form's own live-synced copy, not `original`'s
        // possibly-stale one" reasoning as `RequirementFormState::build_command`.
        test.attachments = self.attachments.iter().cloned().collect();
        test.template = self.template_files.iter().cloned().collect();
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
#[derive(Debug)]
pub struct ResultFormState {
    pub name: String,
    pub title: String,
    pub requirement_path: String,
    pub requirement_commit: String,
    pub test_path: String,
    pub test_commit: String,
    /// See `RequirementFormState::original`'s own doc comment — same
    /// reasoning, preserving `status` (there's no UI to edit it at all
    /// today — it used to silently reset to `StatusV1::default()`,
    /// `Incomplete`, on every save) and `attachment_refs`.
    pub original: Box<ResultDraft>,
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

impl Default for ResultFormState {
    fn default() -> ResultFormState {
        ResultFormState {
            name: String::new(),
            title: String::new(),
            requirement_path: String::new(),
            requirement_commit: String::new(),
            test_path: String::new(),
            test_commit: String::new(),
            original: Box::new(ResultDraft::new(
                String::new(),
                ReferencePath(String::new()),
                String::new(),
                ReferencePath(String::new()),
                String::new(),
            )),
            editing_target: None,
            read_only: false,
            edited: false,
            pending_request: None,
            error: None,
            attachments: Vec::new(),
            new_attachment_path: String::new(),
            local_pool_error: None,
        }
    }
}

impl ResultFormState {
    pub fn build_command(&self, module: Vec<EntryName>, request: RequestId) -> Command {
        let mut result = (*self.original).clone();
        result.title = self.title.clone();
        result.requirement_path = gui_core::ReferencePath(self.requirement_path.clone());
        result.requirement_commit = self.requirement_commit.clone();
        result.test_path = gui_core::ReferencePath(self.test_path.clone());
        result.test_commit = self.test_commit.clone();
        result.attachments = self.attachments.iter().cloned().collect();
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

/// The view/edit page for an already-existing module or the project root
/// (`path: []`) — see `gui_core::ModuleSummary`'s own doc comment for what
/// `summary` carries. Distinct from `ModuleFormState`, which is
/// `AddModule`-only (creation, not viewing/renaming something that already
/// exists) — there's no "editing_target: Option" split here the way the
/// leaf forms have, since a module/project page is only ever reached by
/// selecting something that already exists.
#[derive(Debug, Clone)]
pub struct ModuleDetailFormState {
    /// The module's full path — empty means the project root. Identity
    /// *and* location in one: unlike a requirement/test/result (an
    /// `EntryName` inside a `LogicalPath`), a module has no separate
    /// container to be renamed within, see `Command::RenameModule`'s own
    /// doc comment.
    pub path: Vec<EntryName>,
    /// The name currently shown read-only — kept distinct from `new_name`
    /// so a failed/cancelled edit has something to revert the text field
    /// to without a re-fetch.
    pub display_name: String,
    /// The edit-mode text field.
    pub new_name: String,
    /// `None` while `GetModuleSummary` is in flight.
    pub summary: Option<gui_core::ModuleSummary>,
    pub read_only: bool,
    pub edited: bool,
    pub pending_request: Option<RequestId>,
    pub error: Option<String>,
}

impl ModuleDetailFormState {
    /// `path: []` (the project root) can't go through `RenameModule` — see
    /// `Command::RenameProject`'s own doc comment on why the root needs a
    /// separate command.
    pub fn build_command(&self, request: RequestId) -> Command {
        if self.path.is_empty() {
            Command::RenameProject {
                new_name: self.new_name.clone(),
                request,
            }
        } else {
            Command::RenameModule {
                target: self.path.clone(),
                new_name: EntryName(self.new_name.clone()),
                request,
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeSet;

    use gui_core::{StatusV1, TestReferenceKind};

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

    /// Regression test for the bug `original` fixes: before it existed,
    /// `build_command` built a `RequirementDraft` from scratch, so an
    /// `UpdateRequirement` silently reset every field the form has no UI
    /// for (`tests`, `attachment_refs`, `include_attachments_in_commit`,
    /// `commit`) back to `RequirementDraft::new`'s defaults — wiping a
    /// requirement's own test references just by editing its title.
    #[test]
    fn requirement_form_build_command_preserves_fields_the_form_has_no_ui_for() {
        let mut original = RequirementDraft::new("Old Title");
        original.tests.push(TestReferenceKind::TestReferenceV1(LocalGitReference {
            path: ReferencePath("/tests/generic_test".to_string()),
            commit: "t1".to_string(),
        }));
        original.include_attachments_in_commit = false;
        original.commit = Some("c1".to_string());

        let form = RequirementFormState {
            title: "New Title".to_string(),
            editing_target: Some(LogicalPath::root(EntryName("definition".to_string()))),
            original: Box::new(original),
            ..Default::default()
        };

        let Command::UpdateRequirement { requirement, .. } = form.build_command(Vec::new(), 1) else {
            panic!("expected UpdateRequirement");
        };
        // The actually-edited field changed...
        assert_eq!(requirement.title, "New Title");
        // ...but everything the form has no field for came along
        // unchanged from `original`.
        assert_eq!(requirement.tests.len(), 1);
        assert!(matches!(
            &requirement.tests[0],
            TestReferenceKind::TestReferenceV1(local) if local.path.0 == "/tests/generic_test"
        ));
        assert!(!requirement.include_attachments_in_commit);
        assert_eq!(requirement.commit, Some("c1".to_string()));
    }

    #[test]
    fn requirement_form_build_command_sends_the_forms_own_live_attachments_not_originals() {
        let mut original = RequirementDraft::new("Title");
        // Stale — as of the last `GetEntryDetail`, before some later
        // `AddRequirementAttachment`/`RemoveRequirementAttachment` the
        // form's own `attachments` (not `original.attachments`) reflects.
        original.attachments.insert(PathBuf::from("stale.md"));

        let form = RequirementFormState {
            title: "Title".to_string(),
            editing_target: Some(LogicalPath::root(EntryName("definition".to_string()))),
            original: Box::new(original),
            attachments: vec![PathBuf::from("current.md")],
            ..Default::default()
        };

        let Command::UpdateRequirement { requirement, .. } = form.build_command(Vec::new(), 1) else {
            panic!("expected UpdateRequirement");
        };
        assert_eq!(requirement.attachments, BTreeSet::from([PathBuf::from("current.md")]));
    }

    #[test]
    fn test_form_build_command_preserves_fields_the_form_has_no_ui_for() {
        let mut original = TestDraft::new("Old Title", ResultKindV1::FreeForm);
        original.include_attachments_in_commit = false;
        original.include_template_in_commit = false;
        original.commit = Some("t1".to_string());

        let form = TestFormState {
            title: "New Title".to_string(),
            editing_target: Some(LogicalPath::root(EntryName("generic_test".to_string()))),
            original: Box::new(original),
            attachments: vec![PathBuf::from("checklist.md")],
            template_files: vec![PathBuf::from("result.typ")],
            ..Default::default()
        };

        let Command::UpdateTest { test, .. } = form.build_command(Vec::new(), 1) else {
            panic!("expected UpdateTest");
        };
        assert_eq!(test.title, "New Title");
        assert!(!test.include_attachments_in_commit);
        assert!(!test.include_template_in_commit);
        assert_eq!(test.commit, Some("t1".to_string()));
        assert_eq!(test.attachments, BTreeSet::from([PathBuf::from("checklist.md")]));
        assert_eq!(test.template, BTreeSet::from([PathBuf::from("result.typ")]));
    }

    #[test]
    fn result_form_build_command_preserves_the_status_the_form_has_no_ui_for() {
        let mut original = ResultDraft::new(
            "Old Title",
            ReferencePath("requirements/definition".to_string()),
            "c1",
            ReferencePath("tests/generic_test".to_string()),
            "t1",
        );
        original.status = StatusV1::Pass;

        let form = ResultFormState {
            title: "New Title".to_string(),
            editing_target: Some(LogicalPath::root(EntryName("definition".to_string()))),
            original: Box::new(original),
            ..Default::default()
        };

        let Command::UpdateResult { result, .. } = form.build_command(Vec::new(), 1) else {
            panic!("expected UpdateResult");
        };
        assert_eq!(result.title, "New Title");
        assert!(matches!(result.status, StatusV1::Pass));
    }

    fn module_detail_form(path: Vec<EntryName>, new_name: &str) -> ModuleDetailFormState {
        ModuleDetailFormState {
            path,
            display_name: "whatever".to_string(),
            new_name: new_name.to_string(),
            summary: None,
            read_only: false,
            edited: true,
            pending_request: None,
            error: None,
        }
    }

    #[test]
    fn module_detail_form_build_command_for_a_nested_module_sends_rename_module() {
        let form = module_detail_form(vec![EntryName("setup".to_string())], "renamed");

        let Command::RenameModule { target, new_name, .. } = form.build_command(1) else {
            panic!("expected RenameModule");
        };
        assert_eq!(target, vec![EntryName("setup".to_string())]);
        assert_eq!(new_name, EntryName("renamed".to_string()));
    }

    #[test]
    fn module_detail_form_build_command_for_the_project_root_sends_rename_project() {
        let form = module_detail_form(Vec::new(), "Renamed Project");

        let Command::RenameProject { new_name, .. } = form.build_command(1) else {
            panic!("expected RenameProject");
        };
        assert_eq!(new_name, "Renamed Project");
    }
}
