//! See `README.md` for the design this implements: the egui/eframe layout
//! (menu bar/toolbar/status bar/left tree pane/center pane), why the
//! render thread must never block on `gui-core`, and the exit-prompt state
//! machine.
//!
//! **Status**: the exit-dialog state machine, event-application logic, the
//! layout, and the four per-kind center-pane forms (see `forms.rs`) are
//! all implemented — see this file's `test` module for the logic side
//! (rendering itself isn't unit tested, per README's Testing strategy).
//! Selecting an existing requirement/test/result opens that same form
//! pre-filled (`forms.rs`'s `editing_target`), but read-only
//! (`read_only: true`) by default — a viewer, not the editable form; its
//! own "Edit" button (`editor_edit_clicked`) switches to the editable
//! form for the same entry, itself a real navigation (`NavMode::Edit`)
//! that Back/Forward see like any other — see `apply_entry_detail` and
//! `nav_history`'s doc comment. Each editable form also manages that
//! entry's own local attachment/template pools in place, applied directly
//! rather than via a `GetEntryDetail` round-trip that would discard
//! unsaved edits — see `apply_local_pool_change` and `LocalPoolOp`'s doc
//! comment.

mod config;
#[cfg(all(feature = "debug-panel", debug_assertions))]
mod debug_panel;
mod exit;
mod fonts;
mod forms;
mod icons;
mod recent;
mod theme_colors;
mod view;

pub use config::{GuiConfig, LoadError as ConfigLoadError, ThemeChoice};
pub use exit::ExitDialogState;
pub use fonts::install_icon_font;
pub use forms::{
    AutoCommitKind, DependencyDraft, DependencySlot, ModuleDetailFormState, ModuleFormState, RequirementFormState,
    ResultFormState, TestFormState,
};
pub use recent::{LoadError as RecentLoadError, RecentProjects};

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use gui_core::{
    Command, CoreHandle, EntryDetail, EntryKind, EntryName, Event, LogicalPath, ModulePools, Outcome, RequestId,
    SaveError, TreeNode, TreeSnapshot,
};

/// gui-ui's own state — never a borrow into `gui-core`'s. Populated by
/// applying `Event`s from `CoreHandle::try_recv_event`, replaced wholesale
/// on `Event::TreeChanged`. See README's "Toolkit: egui / eframe".
pub struct GuiApp {
    core: CoreHandle,
    /// `None` until the first `Event::TreeChanged` arrives (e.g. no
    /// project loaded yet).
    tree: Option<TreeSnapshot>,
    selection: Option<LogicalPath>,
    /// The module new entries get created in, and the Attachments dialog
    /// targets — independent of `selection` so a module can be the
    /// "current" one without any leaf being selected. Kept in sync with
    /// `selection` whenever a leaf is clicked (see `select`); `select_module`
    /// sets it directly for a module click. Empty means the project root.
    selected_module: Vec<EntryName>,
    editor: EditorState,
    /// Browser-style selection history for the toolbar's Back/Forward —
    /// `nav_position` is the index of the currently-shown entry. `select()`
    /// and `select_module()` are the two chokepoints every current form of
    /// navigation (a tree leaf click, a module/project-root click) already
    /// goes through, so they're the only things instrumented to grow this
    /// — a future requirement/test/result navigation link gets Back/
    /// Forward for free as long as its own click handler also just calls
    /// one of the two. A `NavTarget::Leaf` entry also carries the
    /// `NavMode` it was viewed in — switching from the read-only viewer
    /// into the editable form (`editor_edit_clicked`) is itself a
    /// navigation, so Back returns to the viewer rather than leaving the
    /// tab on the edit form or jumping past it to whatever was selected
    /// before. Not bounded — entries are small, and capping this wasn't
    /// judged worth the complexity for a first pass.
    nav_history: Vec<NavTarget>,
    nav_position: usize,
    pending: HashMap<RequestId, PendingKind>,
    #[allow(dead_code)]
    status: StatusLine,
    /// Set true whenever an `Outcome` confirms a mutating `Command`
    /// actually changed something, cleared on a successful `Save`'s
    /// `Outcome`. gui-ui's own bookkeeping — it does not ask gui-core
    /// whether there are unsaved changes. See "Exit".
    dirty: bool,
    /// The directory the current project is known to live in — `None`
    /// until a `LoadProject`/`SaveAs` actually succeeds (or after a fresh
    /// `NewProject`, which clears it back to `None`: a brand new project
    /// has no home yet). `gui-ui`'s own bookkeeping, same spirit as
    /// `dirty` — `gui-core` tracks its own `project_path` internally but
    /// never reports it back, since nothing before this needed to know.
    /// Drives whether plain "Save" needs to fall back to a Save As picker
    /// first (`needs_path_before_saving`) and whether Save/Save As are
    /// enabled at all (`self.tree.is_some()` — see `render_toolbar`/
    /// `render_menu_bar`).
    project_path: Option<PathBuf>,
    /// The `(request, path)` a `LoadProject`/`SaveAs` this app itself
    /// sent is waiting to confirm before `project_path` adopts it — a
    /// failed one (or a stale reply for one already superseded by a
    /// later request) must not overwrite `project_path` with a path that
    /// was never actually loaded/saved. Same "ignore stale replies by
    /// request id" shape as `detail_request`/`pools_request`.
    pending_project_path: Option<(RequestId, PathBuf)>,
    exit_dialog: Option<ExitDialogState>,
    /// Set once Stage 2 of the exit flow has actually sent
    /// `Command::Shutdown` — see "Exit" below. From that point on, the
    /// window's own close control must never be intercepted again: the
    /// explicit `ViewportCommand::Close` Stage 2 sends (or, when the OS
    /// close request itself was left uncancelled, the real one) surfaces
    /// as `close_requested()` again on a later pass indistinguishably
    /// from a brand new click, and re-running Stage 1 against it would
    /// cancel that close and reopen the dialog forever.
    shutdown_sent: bool,
    /// Text buffer for the "New Project" modal (the project's name) —
    /// `Some` while it's open. Unlike Open/Save As, this has no
    /// corresponding native picker: a project's *name* isn't a
    /// filesystem path, so there's nothing for `rfd` to pick — see
    /// `Command::NewProject`'s doc comment on why creating one doesn't
    /// touch disk at all until an explicit `SaveAs`.
    new_project_dialog: Option<String>,
    /// Which `GetEntryDetail` request the center pane is waiting on for
    /// the current `selection` — a stale reply for an already-abandoned
    /// selection is ignored rather than overwriting `editor` with detail
    /// for something no longer selected. See `apply_entry_detail`.
    detail_request: Option<RequestId>,
    /// Which `GetRequirementMetStatus` request is refreshing the open
    /// requirement form's `met_status` after a `Validate` completed — same
    /// stale-reply guard as `detail_request`. Only one requirement can be
    /// open at a time, so (unlike `local_pool_ops`) there's never more
    /// than one of these in flight to track. See
    /// `refresh_open_requirement_met_status`.
    met_status_request: Option<RequestId>,
    /// The "Attachments…" modal — `Some` while it's open, for the module
    /// it was opened against (see `new_entry_module_path`, the same
    /// "current module" notion the create forms use).
    attachments_dialog: Option<AttachmentsDialogState>,
    /// Which `GetModulePools` request `attachments_dialog` is waiting on
    /// — same stale-reply guard as `detail_request`.
    pools_request: Option<RequestId>,
    /// The selected module's (or project root's) own attachments/
    /// templates, shown read-only in the tree pane's bottom half — see
    /// `render_selected_module_pane`. Kept separate from
    /// `attachments_dialog`'s own copy: the modal is still the only place
    /// attachments/templates are added or removed, this is purely a
    /// display cache refreshed whenever the selected module or the tree
    /// itself changes.
    sidebar_pools: Option<ModulePools>,
    /// Which `GetModulePools` request `sidebar_pools` is waiting on — same
    /// stale-reply guard as `pools_request`, kept distinct so the modal's
    /// and the sidebar's in-flight fetches never get cross-matched.
    sidebar_pools_request: Option<RequestId>,
    /// In-flight local-pool (a requirement/test/result's own attachments/
    /// template files) add/remove commands, keyed by their `RequestId` —
    /// see `LocalPoolOp`'s doc comment for why this exists instead of
    /// re-fetching `EntryDetail` on completion the way `attachments_dialog`
    /// re-fetches `ModulePools`.
    local_pool_ops: HashMap<RequestId, LocalPoolOp>,
    /// Which `GetModuleSummary` request the open `EditorState::ExistingModule`
    /// page is waiting on — same stale-reply guard as `detail_request`.
    /// Only one module/project page can be open at a time, same "never
    /// more than one in flight" reasoning as `met_status_request`.
    module_summary_request: Option<RequestId>,
    /// The path-picker modal — `Some` while it's open. See
    /// `PathPickerDialogState`'s own doc comment.
    path_picker_dialog: Option<PathPickerDialogState>,
    config: GuiConfig,
    /// Where `config` was loaded from — kept so a zoom click can write
    /// the changed config straight back to the same file (`GuiConfig::
    /// save`), same path `main()` passed to `GuiConfig::load`.
    config_path: PathBuf,
    /// The "Open Recent" submenu's own list — see `recent` module doc
    /// comment and README's "Recently opened projects." Updated (and
    /// written back to `recent_path`) on every successful `LoadProject`/
    /// `SaveAs`/`Save`, same "gui-ui's own bookkeeping, best-effort
    /// persisted" spirit as `config`.
    recent: RecentProjects,
    /// Where `recent` was loaded from/is written back to — same role as
    /// `config_path`.
    recent_path: PathBuf,
    /// The unsaved-changes confirmation prompt — `Some` while it's open,
    /// carrying which project-switching action to resume if the user
    /// continues anyway. See `PendingProjectAction`'s own doc comment.
    unsaved_changes_dialog: Option<PendingProjectAction>,
    /// The unsaved-*form*-edits confirmation prompt — `Some` while it's
    /// open. See `PendingNavigation`'s own doc comment on how this
    /// differs from `unsaved_changes_dialog` above.
    unsaved_form_dialog: Option<PendingNavigation>,
    /// The "must validate before saving" prompt — `Some` while it's open.
    /// Opened when a `Save`/`SaveAs` gui-core can only answer with
    /// `SaveError::NotValidated` comes back (the project is still an
    /// unvalidated `Draft` — e.g. a brand-new `NewProject`, or one edited
    /// since its last `Validate`), rather than leaving that failure
    /// silent — see `ValidateBeforeSaveDialogState`'s own doc comment.
    validate_before_save_dialog: Option<ValidateBeforeSaveDialogState>,
    /// The Delete-button confirmation prompt — `Some` while it's open. See
    /// `DeleteConfirmState`'s own doc comment.
    delete_confirm_dialog: Option<DeleteConfirmState>,
    /// A failed `LoadProject`'s error message — `Some` while the "couldn't
    /// open project" dialog is open. Set from `Outcome::LoadProject`'s
    /// `Err` case (e.g. the target directory isn't a git repository) so
    /// the failure isn't just silently dropped, since `project_path`
    /// deliberately stays unset on a failed load and there's otherwise
    /// nothing on screen to explain why nothing opened.
    load_error_dialog: Option<String>,
    /// The zoom text field's own editable buffer — kept separate from
    /// `config.zoom_percent` (a `u32`) since a text field needs somewhere
    /// to hold invalid/in-progress input (an empty string, a partial
    /// number) that isn't a valid `u32` yet. Resynced from
    /// `config.zoom_percent` on every change that isn't the field itself
    /// being typed into — see `sync_zoom_input`.
    zoom_input: String,
    /// The left pane's filter bar — a substring to match against every
    /// leaf's fully-qualified logical path (`view.rs`'s `leaf_full_path`,
    /// e.g. `/requirements/definition` or
    /// `/modules/setup/tests/generic_test`), case-insensitively. Empty
    /// means unfiltered — every node shows, same as before this existed.
    tree_filter: String,
    /// A one-frame "force every `CollapsingHeader` in the tree open/closed"
    /// signal set by the left pane's Expand All/Collapse All buttons and
    /// consumed (reset to `None`) by the same frame's tree render — see
    /// `view.rs`'s `render_left_pane`. Only a single frame, not a
    /// persistent setting, so a header a user then clicks individually
    /// afterward toggles normally instead of snapping back every frame.
    tree_force_open: Option<bool>,
    /// The debug side panel's own state — see `debug_panel` module doc
    /// comment and README's "Debug side panel" section. Absent entirely,
    /// not just inert, in a non-`debug-panel` build.
    #[cfg(all(feature = "debug-panel", debug_assertions))]
    debug: debug_panel::DebugPanelState,
    /// Monotonic counter for `Command`s this `GuiApp` sends — see
    /// `gui-core`'s README on `RequestId`.
    next_request: RequestId,
}

/// Zoom step/bounds for the status bar's `+`/`−`/Reset controls and the
/// manual-entry field — see `GuiApp::zoom_in_clicked`/`zoom_out_clicked`/
/// `zoom_reset_clicked`/`zoom_input_submitted`. `100` is egui's own
/// unzoomed default, hence also `zoom_reset_clicked`'s target.
const ZOOM_STEP_PERCENT: u32 = 10;
const ZOOM_MIN_PERCENT: u32 = 80;
const ZOOM_MAX_PERCENT: u32 = 400;
const ZOOM_DEFAULT_PERCENT: u32 = 100;

/// Whether a navigation to an existing requirement/test/result lands on
/// its read-only viewer or its editable form — see `nav_history`'s own
/// doc comment and `GuiApp::apply_entry_detail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavMode {
    View,
    Edit,
}

/// One entry in `GuiApp::nav_history` — either a leaf (a requirement/test/
/// result, addressed the same way `select`/`navigate` already do) or a
/// module/project-root selection (`select_module`'s own target, just its
/// path — a module has no `NavMode` of its own to track, see
/// `select_module`'s doc comment on why its own Edit toggle stays a local
/// flag flip rather than a real navigation).
#[derive(Debug, Clone, PartialEq, Eq)]
enum NavTarget {
    Leaf {
        target: LogicalPath,
        kind: EntryKind,
        mode: NavMode,
    },
    Module(Vec<EntryName>),
}

/// Which project-switching action the unsaved-changes prompt
/// (`GuiApp::unsaved_changes_dialog`) interrupted — resumed via
/// `unsaved_changes_confirmed` if the user continues anyway, dropped on
/// Cancel. Covers every entry point that would otherwise discard
/// `self.dirty` content wholesale: starting a brand new project, or
/// opening a different one (whether via the native picker or an "Open
/// Recent" click). Save/Save As don't need this gate — they *resolve*
/// dirty state, they don't discard it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingProjectAction {
    NewProject,
    /// The native folder picker itself is view-layer (`rfd`), not
    /// something this enum can carry — see `unsaved_changes_confirmed`'s
    /// own doc comment on how the two layers hand this back and forth.
    OpenProject,
    OpenRecent(PathBuf),
}

/// Which navigation the unsaved-*form*-edits prompt
/// (`GuiApp::unsaved_form_dialog`) interrupted — resumed via
/// `unsaved_form_dialog_confirmed` if the user continues anyway, dropped
/// on Cancel. Distinct from `PendingProjectAction`: that one guards
/// `self.dirty` (a mutation already applied to `gui-core`, not yet
/// *saved to disk*); this one guards `RequirementFormState`/
/// `TestFormState`/`ResultFormState`'s own `edited` (typed field changes
/// that haven't even been *submitted* to `gui-core` yet — Cancel already
/// discards these on its own, deliberately unprompted, since clicking a
/// button that means "discard" is already the explicit confirmation; see
/// `editor_cancel_clicked`). Every variant here mirrors a real
/// `GuiApp` method that would otherwise silently overwrite/clear
/// `self.editor` out from under an unsaved edit.
/// Which save gui-ui was trying to do when it hit `SaveError::NotValidated`
/// — resumed (as the same `Save`/`SaveAs`) once `ValidateBeforeSaveDialogState
/// ::Asking`'s Validate succeeds. `SaveAs` carries its target path since
/// `pending_project_path` (which normally carries it) is gone by the time
/// the dialog needs to retry — `apply_project_path_result` already took it
/// when the failed `SaveAs` completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingSaveAction {
    Save,
    SaveAs(PathBuf),
}

/// The "must validate before saving" prompt's own state machine — see
/// `GuiApp::validate_before_save_dialog`'s doc comment on when it opens.
/// `Asking` -> `Validating` -> either closes (success — the original
/// `PendingSaveAction` is retried automatically) or `Failed` (shows the
/// validation errors, with a single "Ok" button that just closes the
/// dialog — no auto-retry, since there's nothing to retry that would
/// behave differently).
#[derive(Debug, Clone, PartialEq)]
pub enum ValidateBeforeSaveDialogState {
    Asking { action: PendingSaveAction },
    Validating { request: RequestId, action: PendingSaveAction },
    Failed { errors: Vec<String> },
}

/// What the Delete-confirmation dialog is about to delete — built by
/// `GuiApp::editor_delete_clicked` from whichever edit form is currently
/// open, and turned back into the matching `Command::Remove*` by
/// `GuiApp::delete_confirmed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTarget {
    Requirement(LogicalPath),
    Test(LogicalPath),
    Result(LogicalPath),
    Module(Vec<EntryName>),
}

/// The Delete-button confirmation prompt's own state — see
/// `GuiApp::delete_confirm_dialog`'s doc comment on when it opens.
/// `label` is the entry's display name, shown in the confirmation
/// message. `pending_request` tracks the in-flight `Command::Remove*`
/// once "Delete" is confirmed, so `GuiApp::apply_delete_result` can tell
/// a reply meant for this dialog apart from a stale one for a dialog the
/// user already cancelled — same "stale reply" guard every other
/// in-flight request in this file follows. `error` surfaces the rare
/// case the entry was already gone by the time the command reached
/// gui-core (`Outcome::Remove*(false)`), leaving the dialog open instead
/// of silently closing on a delete that didn't actually happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteConfirmState {
    target: DeleteTarget,
    label: String,
    pending_request: Option<RequestId>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingNavigation {
    Select { target: LogicalPath, kind: EntryKind },
    SelectModule(Vec<EntryName>),
    Back,
    Forward,
    NewRequirement,
    NewTest,
    NewResult,
    NewModule,
}

/// Center-pane form state. See README's "Center pane: distinct forms per
/// kind" — one variant per entity kind, not one generic form.
#[derive(Debug, Default)]
pub enum EditorState {
    #[default]
    None,
    NewRequirement(RequirementFormState),
    NewTest(TestFormState),
    NewResult(ResultFormState),
    NewModule(ModuleFormState),
    /// The view/edit page for an already-existing module or the project
    /// root (`ModuleDetailFormState::path: []`) — see that type's own doc
    /// comment. Distinct from `NewModule`, which stays creation-only.
    ExistingModule(ModuleDetailFormState),
}

/// Which of a requirement/test/result's local pools an in-flight
/// `LocalPoolOp` targets — enough to route a completion back to the right
/// form and field without re-deriving it from the `Outcome` variant (which
/// doesn't carry the path or which form it came from).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalPoolKind {
    RequirementAttachment,
    TestAttachment,
    TestTemplate,
    ResultAttachment,
}

/// A pending local-pool add/remove — tracked per-`RequestId` (unlike the
/// module-level Attachments dialog, which just re-fetches `ModulePools` on
/// any completion) because re-fetching `EntryDetail` here would rebuild
/// the *whole* form via `apply_entry_detail`, discarding any unsaved edits
/// to its other fields (title, text, ...) the user made in the meantime.
/// Instead, a successful op is applied directly to the already-open form's
/// `attachments`/`template_files` list — see `apply_local_pool_change`.
#[derive(Debug, Clone)]
struct LocalPoolOp {
    kind: LocalPoolKind,
    adding: bool,
    /// Which entry this was for — a completion is only applied if the
    /// currently-open form is still editing this same target; otherwise
    /// it's stale (the user navigated away) and ignored.
    target: LogicalPath,
    path: PathBuf,
}

/// The "Attachments…" modal's state — which module it's for, its two
/// pools (once `GetModulePools` replies), and the two text buffers for
/// adding a new path to either pool.
#[derive(Debug, Default)]
pub struct AttachmentsDialogState {
    pub module: Vec<EntryName>,
    pub attachments: Vec<PathBuf>,
    pub templates: Vec<PathBuf>,
    pub new_attachment_path: String,
    pub new_template_path: String,
    /// `true` between opening/refreshing and the `GetModulePools` reply
    /// landing.
    pub loading: bool,
    pub error: Option<String>,
}

/// Where a selection made in the path-picker modal (`PathPickerDialogState`)
/// gets written back to — the field(s) `render_requirement_form`/
/// `render_result_form` opened it from. Each of a requirement/test's
/// fully-qualified path fields across the app funnels through one shared
/// modal rather than each owning its own `egui::ComboBox` (see
/// `GuiApp::path_picker_dialog`'s own doc comment for why), so something
/// has to say which one a given open call means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPickerTarget {
    ResultRequirementPath,
    ResultTestPath,
    /// A requirement's dependency row — `slot` picks which one, same as
    /// `RequirementFormState::pending_commit_fetches`' own keying.
    Dependency(DependencySlot),
}

impl PathPickerTarget {
    /// Which of `self.tree`'s leaf kinds this target's field points at —
    /// every target has exactly one, so `PathPickerDialogState::kind`
    /// derives from it instead of being passed alongside redundantly.
    fn kind(self) -> EntryKind {
        match self {
            PathPickerTarget::ResultRequirementPath | PathPickerTarget::Dependency(_) => EntryKind::Requirement,
            PathPickerTarget::ResultTestPath => EntryKind::Test,
        }
    }
}

/// The path-picker modal's state — open (`Some`) for exactly as long as
/// it's showing. Replaces what used to be a per-field `egui::ComboBox`
/// (one each for the Result form's `requirement_path`/`test_path`, and the
/// Requirement form's dependency `path`): a `ComboBox`'s popup is sized to
/// fit its content on screen, so a project with enough requirements/tests
/// eventually overflows available screen space with no way to search it.
/// A modal has room for a real `ScrollArea` plus a text filter instead,
/// the same shape the left pane's own tree filter (`tree_filter`) already
/// uses for the identical "narrow a long list down by substring" problem
/// — see `render_path_picker_dialog`.
#[derive(Debug, Clone)]
pub struct PathPickerDialogState {
    /// `EntryKind::Requirement` or `EntryKind::Test` — never `Module`/
    /// `Result` (nothing here ever picks a path for either of those).
    pub kind: EntryKind,
    pub target: PathPickerTarget,
    pub filter: String,
}

/// What a given in-flight `RequestId` corresponds to in the UI, so its
/// completion knows which affordance to update (clear a spinner, close the
/// exit dialog, ...).
#[derive(Debug)]
pub enum PendingKind {
    // TODO: Save, Validate, LoadProject, entry-detail fetch, ...
    Generic,
}

#[derive(Debug, Default)]
pub struct StatusLine {
    // TODO: project path, last validation outcome. See README's "Layout".
}

impl GuiApp {
    pub fn new(
        core: CoreHandle,
        config: GuiConfig,
        config_path: PathBuf,
        recent: RecentProjects,
        recent_path: PathBuf,
    ) -> GuiApp {
        GuiApp {
            core,
            tree: None,
            selection: None,
            selected_module: Vec::new(),
            editor: EditorState::default(),
            nav_history: Vec::new(),
            nav_position: 0,
            pending: HashMap::new(),
            status: StatusLine::default(),
            dirty: false,
            project_path: None,
            pending_project_path: None,
            exit_dialog: None,
            shutdown_sent: false,
            new_project_dialog: None,
            detail_request: None,
            met_status_request: None,
            attachments_dialog: None,
            pools_request: None,
            sidebar_pools: None,
            sidebar_pools_request: None,
            local_pool_ops: HashMap::new(),
            module_summary_request: None,
            path_picker_dialog: None,
            zoom_input: config.zoom_percent.to_string(),
            tree_filter: String::new(),
            tree_force_open: None,
            #[cfg(all(feature = "debug-panel", debug_assertions))]
            debug: debug_panel::DebugPanelState::default(),
            config,
            config_path,
            recent,
            recent_path,
            unsaved_changes_dialog: None,
            unsaved_form_dialog: None,
            validate_before_save_dialog: None,
            delete_confirm_dialog: None,
            load_error_dialog: None,
            next_request: 0,
        }
    }

    fn next_request_id(&mut self) -> RequestId {
        self.next_request += 1;
        self.next_request
    }

    /// Updates `self` from one `Event` — the only place gui-core's state
    /// reaches gui-ui's. See README's "Never block the render thread".
    fn apply_event(&mut self, event: Event) {
        match event {
            Event::TreeChanged(tree) => {
                self.tree = Some(tree);
                // Keeps an already-open module/project page's heading in
                // sync with the tree it was read from — matters right
                // after a `LoadProject` in particular: that outcome (and
                // this session's own `select_module(Vec::new())` default-
                // to-root-page response to it) lands *before* its own
                // `TreeChanged`, per `gui-core::Actor::apply_completion`'s
                // ordering, so the page's `display_name` starts as the
                // fallback empty string until this arm corrects it.
                if let EditorState::ExistingModule(form) = &mut self.editor {
                    form.display_name = module_display_name(self.tree.as_ref(), &form.path);
                }
                self.fetch_sidebar_pools(self.selected_module.clone());
            }
            Event::ValidationFailed(_errors) => {
                // TODO: surface in self.status once the status bar exists.
            }
            Event::Completed { request, outcome } => {
                self.pending.remove(&request);
                self.apply_outcome(request, outcome);
            }
        }
    }

    fn apply_outcome(&mut self, request: RequestId, outcome: Outcome) {
        match outcome {
            Outcome::Save(Ok(())) => {
                self.dirty = false;
                // Plain Save re-targets nothing new — `project_path` is
                // already known (Save is only ever enabled once it is,
                // see `needs_path_before_saving`) — but still bumps this
                // project back to the top of "Open Recent", same as
                // `apply_project_path_result` does for Load/Save As.
                if let Some(path) = self.project_path.clone() {
                    self.record_recent_project(path);
                }
            }
            // The project is still an unvalidated `Draft` (a brand-new
            // `NewProject`, or one edited since its last `Validate`) —
            // rather than leaving this silent (there's nothing else that
            // would tell the user *why* nothing got saved), open the
            // "must validate first" prompt with this `Save` as what to
            // retry once validation succeeds.
            Outcome::Save(Err(SaveError::NotValidated)) => {
                self.validate_before_save_dialog =
                    Some(ValidateBeforeSaveDialogState::Asking { action: PendingSaveAction::Save });
            }
            Outcome::SaveAs(result) => {
                if result.is_ok() {
                    self.dirty = false;
                } else if let Err(SaveError::NotValidated) = &result
                    && let Some((pending_request, path)) = &self.pending_project_path
                    && *pending_request == request
                {
                    self.validate_before_save_dialog = Some(ValidateBeforeSaveDialogState::Asking {
                        action: PendingSaveAction::SaveAs(path.clone()),
                    });
                }
                self.apply_project_path_result(request, result.is_ok());
            }
            Outcome::LoadProject(result) => {
                self.apply_project_path_result(request, result.is_ok());
                // Default to the project's own view page — see
                // `Outcome::NewProject`'s note on why `self.tree` isn't
                // necessarily fresh yet here, and `Event::TreeChanged`'s
                // own arm for the fix-up.
                if let Err(error) = &result {
                    self.load_error_dialog = Some(error.to_string());
                } else {
                    self.select_module(Vec::new());
                }
            }
            // Unlike a freshly loaded project (already sitting on disk,
            // nothing to lose by closing), a brand new one has no home at
            // all yet — `NewProject` never touches disk, see `project_path`
            // and `Command::NewProject`'s own doc comments — so closing
            // without a Save As would destroy it outright. Starts dirty so
            // Exit/the OS close button prompt for it just like any other
            // unsaved change, until a real `Save`/`SaveAs` gives it a path.
            // No path either: `NewProject` doesn't go through
            // `pending_project_path` at all (it can't fail — see
            // `Outcome::NewProject`'s own doc comment — so there's no
            // stale-reply race to guard against by waiting for this
            // completion), `new_project_dialog_confirmed` already cleared
            // it up front. Also opens straight to the project's own view
            // page, same as a successful `LoadProject` — unlike that one,
            // `gui-core::Actor::new_project` pushes `TreeChanged` *before*
            // completing (see its own doc comment), so `self.tree` here is
            // already the fresh one and `select_module`'s `display_name`
            // lookup gets the real name immediately, no fix-up needed.
            Outcome::NewProject => {
                self.dirty = true;
                self.select_module(Vec::new());
            }
            // Whether it succeeded or failed, `Validate` can change what
            // an already-open requirement's `met_status` should show — a
            // failed one still demotes the project back to `Draft` (see
            // `a_failed_validate_restores_an_editable_draft` in gui-core),
            // which flips every requirement back to `Unvalidated` just as
            // much as a successful one can flip one to `Met`/`Unmet`. Only
            // `met_status` itself is refreshed, never the rest of the
            // form — see `refresh_open_requirement_met_status`'s own doc
            // comment on why that's safe even mid-edit.
            Outcome::Validate(result) => {
                self.refresh_open_requirement_met_status();
                self.refresh_open_module_summary();
                // If this is the validation `validate_before_save_dialog`
                // itself kicked off (not a plain toolbar "Validate"
                // click, which never populates this dialog), resolve it:
                // success retries the save it was blocking, failure shows
                // the errors in place of the "Validate now?" prompt.
                if let Some(ValidateBeforeSaveDialogState::Validating { request: pending_request, action }) =
                    self.validate_before_save_dialog.clone()
                    && pending_request == request
                {
                    match result {
                        Ok(()) => {
                            self.validate_before_save_dialog = None;
                            match action {
                                PendingSaveAction::Save => self.save_clicked(),
                                PendingSaveAction::SaveAs(path) => self.save_project_as(path),
                            }
                        }
                        Err(errors) => {
                            self.validate_before_save_dialog = Some(ValidateBeforeSaveDialogState::Failed {
                                errors: errors.iter().map(ToString::to_string).collect(),
                            });
                        }
                    }
                }
            }
            // A successful `Undo`/`Redo` changed the project's content —
            // same "unsaved changes" bookkeeping as any other successful
            // mutation. `Err` (nothing to undo/redo) means nothing
            // happened, so `dirty` is left alone either way.
            Outcome::Undo(result) => {
                if result.is_ok() {
                    self.dirty = true;
                }
            }
            Outcome::Redo(result) => {
                if result.is_ok() {
                    self.dirty = true;
                }
            }
            Outcome::AddRequirement(result) => self.apply_create_result(request, result),
            Outcome::UpdateRequirement(result) => self.apply_update_result(request, result),
            Outcome::RefreshStaleTestReferences(result) => {
                self.apply_refresh_stale_test_references_result(request, result)
            }
            Outcome::AddTest(result) => self.apply_create_result(request, result),
            Outcome::UpdateTest(result) => self.apply_update_result(request, result),
            Outcome::AddResult(result) => self.apply_create_result(request, result),
            Outcome::UpdateResult(result) => self.apply_update_result(request, result),
            Outcome::AddModule(result) => self.apply_create_result(request, result),
            Outcome::RemoveRequirement(removed)
            | Outcome::RemoveTest(removed)
            | Outcome::RemoveResult(removed)
            | Outcome::RemoveModule(removed) => self.apply_delete_result(request, removed),
            Outcome::RenameModule(result) => {
                self.apply_module_rename_result(request, result.map_err(|e| e.to_string()))
            }
            Outcome::RenameProject(result) => {
                self.apply_module_rename_result(request, result.map_err(|e| e.to_string()))
            }
            Outcome::AddAttachment(result) => self.apply_pool_change_result(result.map_err(|e| e.to_string())),
            Outcome::AddTemplate(result) => self.apply_pool_change_result(result.map_err(|e| e.to_string())),
            Outcome::RemoveAttachment(removed) | Outcome::RemoveTemplate(removed) => {
                self.apply_pool_change_result(if removed { Ok(()) } else { Err("nothing there to remove".to_string()) })
            }
            Outcome::AddRequirementAttachment(result) => {
                self.apply_local_pool_outcome(request, result.map_err(|e| e.to_string()))
            }
            Outcome::RemoveRequirementAttachment(removed) => self.apply_local_pool_outcome(
                request,
                if removed { Ok(()) } else { Err("nothing there to remove".to_string()) },
            ),
            Outcome::AddTestAttachment(result) => {
                self.apply_local_pool_outcome(request, result.map_err(|e| e.to_string()))
            }
            Outcome::RemoveTestAttachment(removed) => self.apply_local_pool_outcome(
                request,
                if removed { Ok(()) } else { Err("nothing there to remove".to_string()) },
            ),
            Outcome::AddTestTemplateFile(result) => {
                self.apply_local_pool_outcome(request, result.map_err(|e| e.to_string()))
            }
            Outcome::RemoveTestTemplateFile(removed) => self.apply_local_pool_outcome(
                request,
                if removed { Ok(()) } else { Err("nothing there to remove".to_string()) },
            ),
            Outcome::AddResultAttachment(result) => {
                self.apply_local_pool_outcome(request, result.map_err(|e| e.to_string()))
            }
            Outcome::RemoveResultAttachment(removed) => self.apply_local_pool_outcome(
                request,
                if removed { Ok(()) } else { Err("nothing there to remove".to_string()) },
            ),
            // A stale reply for an already-abandoned selection/dialog is
            // ignored, not applied — see `detail_request`'s/`pools_request`'s
            // doc.
            Outcome::EntryDetail(detail) if self.detail_request == Some(request) => {
                self.apply_entry_detail(detail);
            }
            // Same stale-reply guard, and the same "re-check `self.editor`
            // at apply time, not just when the request was sent" nuance —
            // if the user navigated to a *different* requirement while
            // this was in flight, that navigation's own `GetEntryDetail`
            // already set a fresher `met_status`, and this now-stale-
            // target reply is simply dropped instead of overwriting it.
            Outcome::RequirementMetStatus(status) if self.met_status_request == Some(request) => {
                self.met_status_request = None;
                if let EditorState::NewRequirement(form) = &mut self.editor {
                    form.met_status = status;
                }
            }
            Outcome::ModulePools(pools) if self.pools_request == Some(request) => {
                self.apply_module_pools(pools);
            }
            Outcome::ModulePools(pools) if self.sidebar_pools_request == Some(request) => {
                self.apply_sidebar_pools(pools);
            }
            Outcome::ModuleSummary(summary) if self.module_summary_request == Some(request) => {
                self.module_summary_request = None;
                if let EditorState::ExistingModule(form) = &mut self.editor {
                    form.summary = summary;
                }
            }
            Outcome::ResolveLocalCommit(result) => {
                self.apply_commit_fetch_result(request, result.map_err(|e| e.to_string()))
            }
            Outcome::ResolveRemoteCommit(result) => {
                self.apply_commit_fetch_result(request, result.map_err(|e| e.to_string()))
            }
            _ => {}
        }

        // The exit dialog's own in-flight save watches for its own
        // request, regardless of what the outcome actually was — a
        // failed save still means "no longer waiting," per README's
        // Stage 1 ("the save succeeded (or failed — either way it's no
        // longer pending)").
        if let Some(ExitDialogState::Saving {
            request: saving_request,
            ..
        }) = self.exit_dialog
            && saving_request == request
        {
            self.exit_dialog = Some(ExitDialogState::Ready);
        }
    }

    /// Resolves `pending_project_path` for a `LoadProject`/`SaveAs`
    /// completion — adopts the path into `project_path` only if this
    /// completion is for the request that's still actually pending (a
    /// stale reply for an already-superseded request is ignored, same
    /// "ignore stale replies by request id" shape as `apply_entry_detail`
    /// via `detail_request`) and only if it actually succeeded.
    fn apply_project_path_result(&mut self, request: RequestId, succeeded: bool) {
        if self.pending_project_path.as_ref().is_some_and(|(pending_request, _)| *pending_request == request) {
            let (_, path) = self.pending_project_path.take().expect("just checked Some above");
            if succeeded {
                self.project_path = Some(path.clone());
                self.record_recent_project(path);
            }
        }
    }

    /// "Ok" clicked in the "couldn't open project" dialog — just closes it,
    /// same "no state to undo" shape as `validate_before_save_dismissed`'s
    /// `Failed` case.
    fn load_error_dialog_dismissed(&mut self) {
        self.load_error_dialog = None;
    }

    /// Loads the project at `path` — called from the real "Open
    /// Project…" menu item after a native folder picker (`rfd`) returns
    /// one, and directly by tests, which have no native OS dialog to
    /// drive (see README's Testing strategy). `pub` for exactly that
    /// reason: it's this crate's test-support surface for getting a real
    /// project loaded without going through UI that only exists as an
    /// OS-level window outside `egui`'s own accessibility tree.
    pub fn open_project(&mut self, path: PathBuf) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.pending_project_path = Some((request, path.clone()));
        self.send_command(Command::LoadProject { path, request });
    }

    /// Saves the current project to `path` — same "called from a real
    /// picker, callable directly by tests" shape as `open_project`. The
    /// *only* way to give a brand-new (`new_project_dialog_confirmed`)
    /// project a home, and also how an already-loaded one gets
    /// re-targeted ("Save As" in the traditional sense).
    pub fn save_project_as(&mut self, path: PathBuf) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.pending_project_path = Some((request, path.clone()));
        self.send_command(Command::SaveAs { path, request });
    }

    /// `true` when a plain "Save" wouldn't actually have anywhere to
    /// write to — no project known to have a path yet, so the click
    /// handler needs to fall back to the same native picker "Save As…"
    /// uses instead of sending a `Command::Save` gui-core can only answer
    /// with `Outcome::NoProjectLoaded`. See `render_toolbar`'s Save
    /// button and `render_menu_bar`'s File -> Save item.
    pub fn needs_path_before_saving(&self) -> bool {
        self.project_path.is_none()
    }

    fn new_project_dialog_opened(&mut self) {
        self.new_project_dialog = Some(String::new());
    }

    fn new_project_dialog_cancelled(&mut self) {
        self.new_project_dialog = None;
    }

    fn new_project_dialog_confirmed(&mut self) {
        let Some(name) = self.new_project_dialog.take() else {
            return;
        };
        // A brand new project has no home yet — clear immediately rather
        // than waiting for `Outcome::NewProject` (which can't fail, so
        // there's nothing to actually wait for; see that outcome's own
        // doc comment).
        self.project_path = None;
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::NewProject { name, request });
    }

    fn save_clicked(&mut self) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::Save { request });
    }

    fn zoom_in_clicked(&mut self) {
        self.set_zoom_percent(self.config.zoom_percent + ZOOM_STEP_PERCENT);
    }

    fn zoom_out_clicked(&mut self) {
        self.set_zoom_percent(self.config.zoom_percent.saturating_sub(ZOOM_STEP_PERCENT));
    }

    fn zoom_reset_clicked(&mut self) {
        self.set_zoom_percent(ZOOM_DEFAULT_PERCENT);
    }

    /// Applies whatever's currently typed into `zoom_input` — called on
    /// Enter (see `render_status_bar`) or when the field loses focus.
    /// Invalid text (empty, not a number, ...) is silently rejected
    /// rather than surfaced anywhere, same as this app's other silent-
    /// failure precedents (`apply_outcome`'s catch-all) — `sync_zoom_input`
    /// still runs either way, so the field snaps back to whatever the
    /// real current value is instead of being left showing the rejected
    /// text.
    fn zoom_input_submitted(&mut self) {
        if let Ok(value) = self.zoom_input.trim().parse::<u32>() {
            self.set_zoom_percent(value);
        } else {
            self.sync_zoom_input();
        }
    }

    /// Shared by every way of changing the zoom level (`+`/`−`/Reset/
    /// typing a value directly) — clamps to `ZOOM_MIN_PERCENT..=
    /// ZOOM_MAX_PERCENT`, persists, and keeps `zoom_input` (the text
    /// field's own buffer) in sync so it reflects the *actual*, possibly
    /// clamped, value rather than whatever was requested.
    fn set_zoom_percent(&mut self, percent: u32) {
        self.config.zoom_percent = percent.clamp(ZOOM_MIN_PERCENT, ZOOM_MAX_PERCENT);
        self.sync_zoom_input();
        self.persist_config();
    }

    /// Resyncs the zoom text field's buffer to the real current value —
    /// needed after `+`/`−`/Reset change it programmatically (the field
    /// isn't focused then, so nothing else would update it), and after
    /// rejecting or clamping a manually-typed value so the field doesn't
    /// keep showing stale or invalid text. Never called while the field
    /// itself has focus (see `render_status_bar`) — that would overwrite
    /// what the user is actively typing.
    fn sync_zoom_input(&mut self) {
        self.zoom_input = self.config.zoom_percent.to_string();
    }

    /// Best-effort — a failed write here (read-only filesystem, deleted
    /// config directory, ...) shouldn't crash the app or block the zoom
    /// click that triggered it; the new zoom level still applies for the
    /// rest of this session, it just won't survive a restart. Nothing
    /// visible today surfaces the error (same as gui-ui's other silent-
    /// failure precedents — see `apply_outcome`'s catch-all).
    fn persist_config(&self) {
        let _ = self.config.save(&self.config_path);
    }

    /// The status bar's theme selector — same "persist immediately,
    /// best-effort" shape as `set_zoom_percent`. The actual `egui::Context::
    /// set_theme` call happens unconditionally every frame in `ui()`, not
    /// here — this only updates the value it reads from.
    fn theme_selected(&mut self, theme: ThemeChoice) {
        self.config.theme = theme;
        self.persist_config();
    }

    /// Bumps `path` to the front of `recent` and best-effort persists it
    /// — called on every successful `LoadProject`/`SaveAs`/`Save` (see
    /// `apply_project_path_result`/`apply_outcome`'s `Outcome::Save`
    /// arm), so a project's entry in the "Open Recent" submenu always
    /// reflects when it was last opened *or* saved, not just first seen.
    /// Same "best-effort, no visible failure surface" precedent as
    /// `persist_config`.
    fn record_recent_project(&mut self, path: PathBuf) {
        self.recent.record(path);
        let _ = self.recent.save(&self.recent_path);
    }

    /// Opens the unsaved-changes confirmation prompt, remembering which
    /// action to resume if the user continues anyway.
    fn unsaved_changes_dialog_opened(&mut self, action: PendingProjectAction) {
        self.unsaved_changes_dialog = Some(action);
    }

    fn unsaved_changes_dialog_cancelled(&mut self) {
        self.unsaved_changes_dialog = None;
    }

    /// "Continue anyway" — closes the prompt and resumes whichever
    /// action triggered it. `NewProject` opens the name-entry dialog now
    /// (exactly what would have happened without the interruption);
    /// `OpenRecent` loads the project directly. `OpenProject` hands
    /// this back to the caller instead of acting on it here: popping the
    /// native folder picker (`rfd::FileDialog`) is view-layer, the same
    /// reason it's `render_menu_bar`'s own click handler that calls it
    /// for the *not-dirty* path too, rather than a `GuiApp` method — so
    /// `render_unsaved_changes_dialog` is the one place both paths funnel
    /// through.
    fn unsaved_changes_confirmed(&mut self) -> Option<PendingProjectAction> {
        let action = self.unsaved_changes_dialog.take()?;
        match &action {
            PendingProjectAction::NewProject => self.new_project_dialog_opened(),
            PendingProjectAction::OpenRecent(path) => self.open_project(path.clone()),
            PendingProjectAction::OpenProject => {}
        }
        Some(action)
    }

    /// Whether the currently-open form has any unsubmitted field/
    /// dependency edits — always `false` for the read-only viewer (see
    /// `RequirementFormState::edited`'s own doc comment), a create-mode
    /// form that hasn't been touched, a module's form (not tracked, see
    /// `PendingNavigation`'s doc comment on scope), or nothing open at
    /// all. `view.rs`'s click handlers for every navigation that would
    /// discard `self.editor` check this first, same plain-field-read
    /// pattern `self.dirty` already follows for `PendingProjectAction`'s
    /// gate.
    fn editor_has_unsaved_edits(&self) -> bool {
        match &self.editor {
            EditorState::NewRequirement(f) => f.edited,
            EditorState::NewTest(f) => f.edited,
            EditorState::NewResult(f) => f.edited,
            EditorState::ExistingModule(f) => f.edited,
            EditorState::NewModule(_) | EditorState::None => false,
        }
    }

    fn unsaved_form_dialog_opened(&mut self, action: PendingNavigation) {
        self.unsaved_form_dialog = Some(action);
    }

    fn unsaved_form_dialog_cancelled(&mut self) {
        self.unsaved_form_dialog = None;
    }

    /// "Continue anyway" — closes the prompt and performs whichever
    /// navigation triggered it. Unlike `unsaved_changes_confirmed`, every
    /// variant here is a plain `GuiApp` method call with nothing for the
    /// view layer to do afterward (no native picker involved), so this
    /// just runs it directly rather than handing anything back.
    fn unsaved_form_dialog_confirmed(&mut self) {
        let Some(action) = self.unsaved_form_dialog.take() else {
            return;
        };
        match action {
            PendingNavigation::Select { target, kind } => self.select(target, kind),
            PendingNavigation::SelectModule(module) => self.select_module(module),
            PendingNavigation::Back => self.back_clicked(),
            PendingNavigation::Forward => self.forward_clicked(),
            PendingNavigation::NewRequirement => self.new_requirement_clicked(),
            PendingNavigation::NewTest => self.new_test_clicked(),
            PendingNavigation::NewResult => self.new_result_clicked(),
            PendingNavigation::NewModule => self.new_module_clicked(),
        }
    }

    fn validate_clicked(&mut self) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::Validate { request });
    }

    /// "Validate" clicked in `ValidateBeforeSaveDialogState::Asking` —
    /// sends the same `Command::Validate` `validate_clicked` does, but
    /// tracked separately (`Validating`, carrying the `PendingSaveAction`
    /// to resume) so `apply_outcome`'s `Outcome::Validate` arm knows this
    /// completion is the dialog's own, not an unrelated toolbar click.
    fn validate_before_save_confirmed(&mut self) {
        let Some(ValidateBeforeSaveDialogState::Asking { action }) = self.validate_before_save_dialog.clone() else {
            return;
        };
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.validate_before_save_dialog = Some(ValidateBeforeSaveDialogState::Validating { request, action });
        self.send_command(Command::Validate { request });
    }

    /// "Cancel" clicked in `ValidateBeforeSaveDialogState::Asking`, or
    /// "Ok" clicked in `::Failed` — either way there's nothing left to
    /// resume, so this just closes the dialog.
    fn validate_before_save_dismissed(&mut self) {
        self.validate_before_save_dialog = None;
    }

    /// Re-fetches just the currently-open requirement's `met_status` —
    /// called after a `Validate` completes (see `apply_outcome`'s
    /// `Outcome::Validate` arm). A no-op if nothing's open, or what's
    /// open isn't a requirement (a module/test/result form has no
    /// `met_status` to refresh at all). Deliberately narrower than
    /// `select_from_history`'s "always re-fetch everything" convention:
    /// `met_status` is the only field `Validate` can actually change, and
    /// it's never something the user types into, so refreshing just it
    /// carries none of a full `GetEntryDetail` re-fetch's risk of
    /// clobbering unsaved edits sitting in the rest of the form.
    fn refresh_open_requirement_met_status(&mut self) {
        let EditorState::NewRequirement(form) = &self.editor else {
            return;
        };
        let Some(target) = form.editing_target.clone() else {
            return;
        };
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.met_status_request = Some(request);
        self.send_command(Command::GetRequirementMetStatus { target, request });
    }

    /// The module/project page's counterpart to
    /// `refresh_open_requirement_met_status` — a `Validate` can change the
    /// met/unmet and pass/fail/incomplete counts an open page's `summary`
    /// shows, whether it succeeded (fresh resolved data) or failed (the
    /// project's back to `Draft`, so `summary.validated` goes back to
    /// `false` — same "still changes what's shown" reasoning that
    /// requirement-side refresh already documents). A no-op if nothing's
    /// open, or what's open isn't a module/project page.
    fn refresh_open_module_summary(&mut self) {
        let EditorState::ExistingModule(form) = &self.editor else {
            return;
        };
        let module = form.path.clone();
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.module_summary_request = Some(request);
        self.send_command(Command::GetModuleSummary { module, request });
    }

    /// The requirement viewer's "Update Stale References" button
    /// (`has_stale_test_reference` gates whether it's even shown). Reuses
    /// `form.pending_request`/`form.error` — the same fields Save/Create
    /// use — rather than adding dedicated ones: this button only ever
    /// shows in the read-only viewer, which never has a Save/Create of
    /// its own in flight, so there's no risk of the two colliding.
    fn refresh_stale_test_references_clicked(&mut self) {
        let target = {
            let EditorState::NewRequirement(form) = &self.editor else {
                return;
            };
            let Some(target) = form.editing_target.clone() else {
                return;
            };
            target
        };
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        if let EditorState::NewRequirement(form) = &mut self.editor {
            form.pending_request = Some(request);
            form.error = None;
        }
        self.send_command(Command::RefreshStaleTestReferences { target, request });
    }

    fn undo_clicked(&mut self) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::Undo { request });
    }

    fn redo_clicked(&mut self) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::Redo { request });
    }

    /// The one place every `Command` (except `Shutdown` — see `ui`'s own
    /// comment on why that one bypasses this) is actually sent — routes
    /// through the debug panel's Tx stall/failure injection when the
    /// `debug-panel` feature is enabled, logging every message either
    /// way in that build; a plain passthrough to `CoreHandle::send`
    /// otherwise.
    fn send_command(&mut self, command: Command) {
        #[cfg(all(feature = "debug-panel", debug_assertions))]
        let Some(command) = self.debug.on_tx(command) else {
            return;
        };
        self.core.send(command);
    }

    /// The one place every `Event` is drained from `gui-core` — logs
    /// each one (debug builds only) before applying it, and — while an
    /// Rx stall is active — skips draining `try_recv_event` at all, so
    /// real events simply queue up in the channel itself rather than
    /// gui-ui ever seeing them late; `gui-core`'s own unbounded channel
    /// (see its README) is exactly what makes that safe to do for a
    /// while without anything being lost.
    fn poll_events(&mut self) {
        #[cfg(all(feature = "debug-panel", debug_assertions))]
        if self.debug.is_rx_stalled(Instant::now()) {
            return;
        }
        while let Some(event) = self.core.try_recv_event() {
            #[cfg(all(feature = "debug-panel", debug_assertions))]
            self.debug.log_rx(&event);
            self.apply_event(event);
        }
    }

    /// Called once per frame (see `ui`) — once an active Tx stall has
    /// elapsed, actually forwards whatever `Command`s built up behind it
    /// to `CoreHandle::send`, in the order they were originally sent.
    #[cfg(all(feature = "debug-panel", debug_assertions))]
    fn flush_stalled_tx(&mut self) {
        for command in self.debug.release_stalled_tx(Instant::now()) {
            self.core.send(command);
        }
    }

    /// The debug button in the menu bar's top right corner — opening
    /// needs confirmation (`debug_confirm_...`), closing (this button,
    /// clicked again once the panel is already open) doesn't: asking
    /// "are you sure" makes sense for something diagnostic a normal user
    /// shouldn't stumble into, but asking it again just to close what's
    /// already open would only be friction. See README's "Planned: debug
    /// side panel" (now implemented).
    #[cfg(all(feature = "debug-panel", debug_assertions))]
    fn debug_panel_button_clicked(&mut self) {
        if self.debug.open {
            self.debug.open = false;
        } else {
            self.debug.confirm_open = true;
        }
    }

    #[cfg(all(feature = "debug-panel", debug_assertions))]
    fn debug_confirm_opened_clicked(&mut self) {
        self.debug.confirm_open = false;
        self.debug.open = true;
    }

    #[cfg(all(feature = "debug-panel", debug_assertions))]
    fn debug_confirm_cancelled_clicked(&mut self) {
        self.debug.confirm_open = false;
    }

    /// A tree node was clicked — same job as `navigate`, always in
    /// `NavMode::View` (a fresh click into the tree always lands on the
    /// read-only viewer first, never straight into the editable form —
    /// see `apply_entry_detail`).
    fn select(&mut self, target: LogicalPath, kind: EntryKind) {
        self.navigate(target, kind, NavMode::View);
    }

    /// The one chokepoint every current form of leaf navigation goes
    /// through — a tree leaf click (`select`), the viewer's "Edit" button
    /// (`editor_edit_clicked`), and Cancel-from-an-existing-entry's-edit
    /// (`editor_cancel_clicked`) all just call this with the mode they
    /// mean. Truncates any "forward" history past the current position
    /// and pushes a `NavTarget::Leaf` (the usual "a fresh navigation
    /// invalidates forward history" rule browsers follow), then advances
    /// to it. `select_from_history` is the twin that skips the
    /// truncate-and-push, for Back/Forward re-visiting an entry already in
    /// `nav_history`. `select_module` is the module/project-root
    /// counterpart of this function.
    fn navigate(&mut self, target: LogicalPath, kind: EntryKind, mode: NavMode) {
        self.nav_history.truncate(self.nav_position + 1);
        self.nav_history.push(NavTarget::Leaf {
            target: target.clone(),
            kind,
            mode,
        });
        self.nav_position = self.nav_history.len() - 1;
        self.select_from_history(target, kind);
    }

    /// Sets the selection, closes whatever form was open (a stale one
    /// would otherwise linger while the new selection's detail is still
    /// loading), and fetches its detail — everything `navigate` does
    /// *except* touching `nav_history`/`nav_position`. Back/Forward
    /// (`back_clicked`/`forward_clicked`) call this directly after
    /// moving `nav_position` themselves, since re-visiting a history
    /// entry must not *also* count as a new navigation — doing so would
    /// immediately make Forward available again pointing at where the
    /// user just came from, breaking the usual back/forward mental
    /// model. Always re-fetches rather than trusting whatever's already
    /// in `editor` — same reasoning as every other navigation here:
    /// simpler and always-correct beats trying to locally toggle a stale
    /// form between view/edit, at the cost of a round trip Back/Forward
    /// (and the Edit button, and Cancel) all now take.
    ///
    /// `kind` matters: a requirement, test, and result can share a name
    /// within the same module (e.g. a result named after the requirement
    /// it reports on), so `GetEntryDetail` needs to know which pool to
    /// resolve `target.name` against rather than guessing.
    fn select_from_history(&mut self, target: LogicalPath, kind: EntryKind) {
        self.selected_module = target.modules.clone();
        self.selection = Some(target.clone());
        self.editor = EditorState::None;
        let request = self.next_request_id();
        self.detail_request = Some(request);
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::GetEntryDetail { target, kind, request });
    }

    /// The `NavMode` of the entry `nav_position` currently points at —
    /// what `apply_entry_detail` builds the next-landing form in. Falls
    /// back to `View` when `nav_history` is empty, or the entry there is a
    /// `NavTarget::Module` (this function is only actually consulted right
    /// after a `NavTarget::Leaf` lands, since `apply_entry_detail` is only
    /// reached via a leaf's own `GetEntryDetail` reply — kept total rather
    /// than panicking, same "never panic on a momentarily-inapplicable
    /// view" spirit `module_display_name`'s own fallback follows).
    fn current_nav_mode(&self) -> NavMode {
        match self.nav_history.get(self.nav_position) {
            Some(NavTarget::Leaf { mode, .. }) => *mode,
            _ => NavMode::View,
        }
    }

    fn can_go_back(&self) -> bool {
        self.nav_position > 0
    }

    fn can_go_forward(&self) -> bool {
        self.nav_position + 1 < self.nav_history.len()
    }

    fn back_clicked(&mut self) {
        if !self.can_go_back() {
            return;
        }
        self.nav_position -= 1;
        self.select_from_nav_target(self.nav_history[self.nav_position].clone());
    }

    fn forward_clicked(&mut self) {
        if !self.can_go_forward() {
            return;
        }
        self.nav_position += 1;
        self.select_from_nav_target(self.nav_history[self.nav_position].clone());
    }

    /// Dispatches a landed-on history entry to whichever of
    /// `select_from_history`/`select_module_from_history` applies —
    /// shared by `back_clicked`/`forward_clicked`, the only two places
    /// that ever jump straight to an already-recorded `NavTarget` rather
    /// than creating a fresh one.
    fn select_from_nav_target(&mut self, target: NavTarget) {
        match target {
            NavTarget::Leaf { target, kind, .. } => self.select_from_history(target, kind),
            NavTarget::Module(module) => self.select_module_from_history(module),
        }
    }

    /// A module (or the project root, `module: []`) tree node was clicked
    /// — same job `navigate` does for a leaf: truncates any "forward"
    /// history past the current position, pushes a fresh `NavTarget::
    /// Module` (the module/project-root counterpart of `navigate`'s own
    /// `NavTarget::Leaf` push), advances to it, then hands off to
    /// `select_module_from_history` to actually do the work.
    fn select_module(&mut self, module: Vec<EntryName>) {
        self.nav_history.truncate(self.nav_position + 1);
        self.nav_history.push(NavTarget::Module(module.clone()));
        self.nav_position = self.nav_history.len() - 1;
        self.select_module_from_history(module);
    }

    /// Sets `selected_module`, clears the leaf `selection` (same as
    /// `select_from_history` does, so the center pane doesn't keep showing
    /// a stale leaf's form for a module that's now current), and opens its
    /// view page: `GetModuleSummary` for the counts, `display_name` taken
    /// from the tree (already in hand, no separate fetch needed for that
    /// part — see `module_display_name`). Everything `select_module` does
    /// *except* touching `nav_history`/`nav_position` — the module
    /// counterpart of `select_from_history`, used directly by
    /// `select_from_nav_target` for Back/Forward re-visiting a module
    /// already in `nav_history`.
    fn select_module_from_history(&mut self, module: Vec<EntryName>) {
        self.selected_module = module.clone();
        self.selection = None;
        self.fetch_sidebar_pools(module.clone());
        let display_name = module_display_name(self.tree.as_ref(), &module);
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.module_summary_request = Some(request);
        self.editor = EditorState::ExistingModule(ModuleDetailFormState {
            path: module.clone(),
            display_name,
            new_name: String::new(),
            summary: None,
            read_only: true,
            edited: false,
            pending_request: None,
            error: None,
        });
        self.send_command(Command::GetModuleSummary { module, request });
    }

    /// Opens the path-picker modal — the Result form's "Pick…" buttons and
    /// the Requirement form's dependency-row "Pick…" button all funnel
    /// through here. `target` decides both where a selection gets written
    /// back to and (via `PathPickerTarget::kind`) which of `self.tree`'s
    /// leaves the modal lists. Starts with an empty filter every time —
    /// reopening for a different field shouldn't carry over whatever was
    /// typed into a previous, unrelated search.
    fn path_picker_dialog_opened(&mut self, target: PathPickerTarget) {
        self.path_picker_dialog = Some(PathPickerDialogState {
            kind: target.kind(),
            target,
            filter: String::new(),
        });
    }

    fn path_picker_dialog_cancelled(&mut self) {
        self.path_picker_dialog = None;
    }

    /// A row picked in the modal's list — writes the fully-qualified
    /// reference path string for `picked` into whichever field
    /// `path_picker_dialog.target` names, then closes the modal. A no-op
    /// (but still closes) if the target form has since closed or switched
    /// to a different entry — same "a stale target is simply dropped"
    /// precedent `apply_local_pool_change` follows.
    fn path_picker_dialog_selected(&mut self, picked: LogicalPath) {
        let Some(dialog) = self.path_picker_dialog.take() else {
            return;
        };
        let path_str = absolute_reference_path(&picked, leaf_kind_segment(dialog.kind));
        match dialog.target {
            PathPickerTarget::ResultRequirementPath => {
                if let EditorState::NewResult(form) = &mut self.editor {
                    form.requirement_path = path_str;
                    form.edited = true;
                }
            }
            PathPickerTarget::ResultTestPath => {
                if let EditorState::NewResult(form) = &mut self.editor {
                    form.test_path = path_str;
                    form.edited = true;
                }
            }
            PathPickerTarget::Dependency(slot) => {
                if let EditorState::NewRequirement(form) = &mut self.editor {
                    let dep = match slot {
                        DependencySlot::Existing(i) => form.dependencies.get_mut(i),
                        DependencySlot::New => Some(&mut form.new_dependency),
                    };
                    if let Some(DependencyDraft::LocalRequirement { path, .. }) = dep {
                        *path = path_str;
                    }
                    // Same "only an already-added row is part of the
                    // form's real content" distinction
                    // `apply_commit_fetch_result` already draws.
                    if let DependencySlot::Existing(_) = slot {
                        form.edited = true;
                    }
                }
            }
        }
    }

    /// The module/project page's Save — a reply for an already-closed (or
    /// navigated-away-from) page is ignored, same "stale reply" shape as
    /// `detail_request`. Success flips the page back to its read-only view
    /// (mirroring `select_from_history`'s "always land on the viewer"
    /// convention), and updates `path`/`selected_module`/`display_name` to
    /// the new name — for the root (`path: []`), `RenameProject` never
    /// touches the path itself, so `last_mut` below is simply a no-op
    /// there, no special-casing needed. No `GetModuleSummary` re-fetch: a
    /// rename can't change the subtree's counts, only its own name.
    fn apply_module_rename_result(&mut self, request: RequestId, result: Result<(), String>) {
        let EditorState::ExistingModule(form) = &mut self.editor else {
            return;
        };
        if form.pending_request != Some(request) {
            return;
        }
        form.pending_request = None;

        match result {
            Ok(()) => {
                self.dirty = true;
                let mut new_path = form.path.clone();
                if let Some(last) = new_path.last_mut() {
                    *last = EntryName(form.new_name.clone());
                }
                form.path = new_path.clone();
                form.display_name = form.new_name.clone();
                form.read_only = true;
                form.edited = false;
                form.error = None;
                self.selected_module = new_path;
            }
            Err(message) => {
                form.error = Some(message);
            }
        }
    }

    /// Populates `editor` with the matching form, pre-filled and in
    /// `editing_target: Some(target)` mode — see `forms.rs`'s module doc
    /// comment. `read_only` comes from `current_nav_mode()`: a plain tree
    /// click (`NavMode::View`) lands on the read-only viewer, the "Edit"
    /// button or Forward-into-an-edit-entry (`NavMode::Edit`) lands on the
    /// editable form — both went through the exact same `GetEntryDetail`
    /// round trip via `navigate`/`select_from_history`, so this is the one
    /// place that decides which of the two `render_*_form` actually
    /// shows. `None` (nothing found — e.g. the entry was removed by
    /// someone/something else between selecting it and this reply
    /// arriving) closes the editor rather than showing a stale form.
    fn apply_entry_detail(&mut self, detail: Option<EntryDetail>) {
        let Some(target) = self.selection.clone() else {
            return;
        };
        let read_only = self.current_nav_mode() == NavMode::View;
        self.editor = match detail {
            None => EditorState::None,
            Some(EntryDetail::Requirement {
                title,
                requirement_text,
                requirement_guidance,
                test_guidance,
                dependencies,
                attachments,
                met_status,
                original,
            }) => EditorState::NewRequirement(RequirementFormState {
                name: target.name.as_str().to_string(),
                title,
                requirement_text,
                requirement_guidance: requirement_guidance.unwrap_or_default(),
                test_guidance: test_guidance.unwrap_or_default(),
                original,
                met_status,
                editing_target: Some(target),
                read_only,
                edited: false,
                pending_request: None,
                error: None,
                dependencies: dependencies.into_iter().map(DependencyDraft::from_core).collect(),
                new_dependency: DependencyDraft::default(),
                attachments,
                new_attachment_path: String::new(),
                local_pool_error: None,
                pending_commit_fetches: HashMap::new(),
                commit_fetch_error: None,
            }),
            Some(EntryDetail::Test {
                title,
                result_kind,
                attachments,
                template_files,
                original,
            }) => EditorState::NewTest(TestFormState {
                name: target.name.as_str().to_string(),
                title,
                result_kind,
                original,
                editing_target: Some(target),
                read_only,
                edited: false,
                pending_request: None,
                error: None,
                attachments,
                new_attachment_path: String::new(),
                template_files,
                new_template_path: String::new(),
                local_pool_error: None,
            }),
            Some(EntryDetail::Result {
                title,
                requirement_path,
                requirement_commit,
                test_path,
                test_commit,
                attachments,
                original,
            }) => EditorState::NewResult(ResultFormState {
                name: target.name.as_str().to_string(),
                title,
                requirement_path,
                requirement_commit,
                test_path,
                test_commit,
                original,
                editing_target: Some(target),
                read_only,
                edited: false,
                pending_request: None,
                error: None,
                attachments,
                new_attachment_path: String::new(),
                local_pool_error: None,
            }),
        };
    }

    /// Where a new entry gets created, and what the Attachments dialog
    /// targets — just `selected_module`, kept in sync by `select`/
    /// `select_module` regardless of whether a leaf or a module was
    /// clicked last.
    fn new_entry_module_path(&self) -> Vec<EntryName> {
        self.selected_module.clone()
    }

    fn new_requirement_clicked(&mut self) {
        self.editor = EditorState::NewRequirement(RequirementFormState::default());
    }

    fn new_test_clicked(&mut self) {
        self.editor = EditorState::NewTest(TestFormState::default());
    }

    fn new_result_clicked(&mut self) {
        self.editor = EditorState::NewResult(ResultFormState::default());
    }

    fn new_module_clicked(&mut self) {
        self.editor = EditorState::NewModule(ModuleFormState::default());
    }

    /// Opens the Attachments modal for the current module (same
    /// `new_entry_module_path` notion the create forms use) and fetches
    /// its pools.
    fn attachments_dialog_opened(&mut self) {
        let module = self.new_entry_module_path();
        self.attachments_dialog = Some(AttachmentsDialogState {
            module: module.clone(),
            loading: true,
            ..AttachmentsDialogState::default()
        });
        self.fetch_module_pools(module);
    }

    fn fetch_module_pools(&mut self, module: Vec<EntryName>) {
        let request = self.next_request_id();
        self.pools_request = Some(request);
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::GetModulePools { module, request });
    }

    fn attachments_dialog_closed(&mut self) {
        self.attachments_dialog = None;
    }

    /// `None` (module not found — can happen if it was removed while the
    /// dialog was open) closes the dialog rather than showing a stale/
    /// broken one.
    fn apply_module_pools(&mut self, pools: Option<ModulePools>) {
        let Some(dialog) = &mut self.attachments_dialog else {
            return;
        };
        match pools {
            None => self.attachments_dialog = None,
            Some(pools) => {
                dialog.attachments = pools.attachments;
                dialog.templates = pools.templates;
                dialog.loading = false;
                dialog.error = None;
            }
        }
    }

    /// Refreshes `sidebar_pools` for `module` — called whenever the
    /// selected module changes (`select_module_from_history`) and whenever
    /// the tree itself changes (`Event::TreeChanged`, which also covers
    /// the modal's own add/remove commands completing).
    fn fetch_sidebar_pools(&mut self, module: Vec<EntryName>) {
        let request = self.next_request_id();
        self.sidebar_pools_request = Some(request);
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::GetModulePools { module, request });
    }

    /// `None` (module not found — can happen if the selected module was
    /// just deleted) simply clears the sidebar's cache rather than
    /// closing anything, since unlike `attachments_dialog` there's no
    /// modal here to close; `render_selected_module_pane` already handles
    /// a since-deleted selection via `resolve_tree_module` returning
    /// `None` for its own label.
    fn apply_sidebar_pools(&mut self, pools: Option<ModulePools>) {
        self.sidebar_pools = pools;
    }

    /// Shared by all four pool-mutating outcomes
    /// (`AddAttachment`/`AddTemplate`/`RemoveAttachment`/`RemoveTemplate`):
    /// success marks the project dirty and re-fetches the pools so the
    /// dialog's lists reflect what actually happened; failure reports
    /// inline instead. No per-request pending-tracking on these — a
    /// mutation's own completion is self-evident from the refreshed list
    /// (or the error). **Known gap**: if the dialog is closed and reopened
    /// for a *different* module before an in-flight mutation's reply
    /// lands, that reply gets applied to whatever dialog happens to be
    /// open then (a spurious refetch/error, not data corruption — nothing
    /// wrong gets written to disk) rather than being recognized as stale.
    /// Narrow enough, and `detail_request`/`pools_request`-style
    /// per-request tracking heavy enough, that it's not worth closing this
    /// pass.
    fn apply_pool_change_result(&mut self, result: Result<(), String>) {
        let Some(dialog) = &mut self.attachments_dialog else {
            return;
        };
        match result {
            Ok(()) => {
                self.dirty = true;
                let module = dialog.module.clone();
                self.fetch_module_pools(module);
            }
            Err(message) => dialog.error = Some(message),
        }
    }

    fn attachments_dialog_add_attachment_clicked(&mut self) {
        let Some(dialog) = &mut self.attachments_dialog else {
            return;
        };
        if dialog.new_attachment_path.trim().is_empty() {
            return;
        }
        let module = dialog.module.clone();
        let path = PathBuf::from(std::mem::take(&mut dialog.new_attachment_path));
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::AddAttachment { module, path, request });
    }

    fn attachments_dialog_remove_attachment_clicked(&mut self, path: PathBuf) {
        let Some(dialog) = &self.attachments_dialog else {
            return;
        };
        let module = dialog.module.clone();
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::RemoveAttachment { module, path, request });
    }

    fn attachments_dialog_add_template_clicked(&mut self) {
        let Some(dialog) = &mut self.attachments_dialog else {
            return;
        };
        if dialog.new_template_path.trim().is_empty() {
            return;
        }
        let module = dialog.module.clone();
        let path = PathBuf::from(std::mem::take(&mut dialog.new_template_path));
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::AddTemplate { module, path, request });
    }

    fn attachments_dialog_remove_template_clicked(&mut self, path: PathBuf) {
        let Some(dialog) = &self.attachments_dialog else {
            return;
        };
        let module = dialog.module.clone();
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.send_command(Command::RemoveTemplate { module, path, request });
    }

    /// Sends the `Add*` command for `kind` and remembers it in
    /// `local_pool_ops` so the reply can update the right form/field.
    fn add_local_pool_entry(&mut self, kind: LocalPoolKind, target: LogicalPath, path: PathBuf) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.local_pool_ops.insert(
            request,
            LocalPoolOp {
                kind,
                adding: true,
                target: target.clone(),
                path: path.clone(),
            },
        );
        let command = match kind {
            LocalPoolKind::RequirementAttachment => Command::AddRequirementAttachment { target, path, request },
            LocalPoolKind::TestAttachment => Command::AddTestAttachment { target, path, request },
            LocalPoolKind::TestTemplate => Command::AddTestTemplateFile { target, path, request },
            LocalPoolKind::ResultAttachment => Command::AddResultAttachment { target, path, request },
        };
        self.send_command(command);
    }

    fn remove_local_pool_entry(&mut self, kind: LocalPoolKind, target: LogicalPath, path: PathBuf) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        self.local_pool_ops.insert(
            request,
            LocalPoolOp {
                kind,
                adding: false,
                target: target.clone(),
                path: path.clone(),
            },
        );
        let command = match kind {
            LocalPoolKind::RequirementAttachment => Command::RemoveRequirementAttachment { target, path, request },
            LocalPoolKind::TestAttachment => Command::RemoveTestAttachment { target, path, request },
            LocalPoolKind::TestTemplate => Command::RemoveTestTemplateFile { target, path, request },
            LocalPoolKind::ResultAttachment => Command::RemoveResultAttachment { target, path, request },
        };
        self.send_command(command);
    }

    /// The Add button for whichever form's local-pool section is showing.
    /// Reads the matching field (`new_attachment_path`/`new_template_path`
    /// per `kind`) off whichever form is currently open, clears it on
    /// send, and does nothing if the field is empty or the form isn't
    /// actually in edit mode (there's no `editing_target` to send against
    /// — shouldn't happen given the section is only rendered when
    /// editing, but this is the defensive fallback if it's ever called
    /// otherwise).
    fn local_attachment_add_clicked(&mut self, kind: LocalPoolKind) {
        let Some((target, path)) = (match (&mut self.editor, kind) {
            (EditorState::NewRequirement(form), LocalPoolKind::RequirementAttachment) => form
                .editing_target
                .clone()
                .map(|target| (target, &mut form.new_attachment_path)),
            (EditorState::NewTest(form), LocalPoolKind::TestAttachment) => form
                .editing_target
                .clone()
                .map(|target| (target, &mut form.new_attachment_path)),
            (EditorState::NewTest(form), LocalPoolKind::TestTemplate) => form
                .editing_target
                .clone()
                .map(|target| (target, &mut form.new_template_path)),
            (EditorState::NewResult(form), LocalPoolKind::ResultAttachment) => form
                .editing_target
                .clone()
                .map(|target| (target, &mut form.new_attachment_path)),
            _ => None,
        })
        .filter(|(_, field)| !field.trim().is_empty())
        .map(|(target, field)| (target, PathBuf::from(std::mem::take(field)))) else {
            return;
        };
        self.add_local_pool_entry(kind, target, path);
    }

    fn local_attachment_remove_clicked(&mut self, kind: LocalPoolKind, path: PathBuf) {
        let target = match (&self.editor, kind) {
            (EditorState::NewRequirement(form), LocalPoolKind::RequirementAttachment) => form.editing_target.clone(),
            (EditorState::NewTest(form), LocalPoolKind::TestAttachment | LocalPoolKind::TestTemplate) => {
                form.editing_target.clone()
            }
            (EditorState::NewResult(form), LocalPoolKind::ResultAttachment) => form.editing_target.clone(),
            _ => None,
        };
        let Some(target) = target else {
            return;
        };
        self.remove_local_pool_entry(kind, target, path);
    }

    /// A dependency row's "Auto" button — see `render_dependency_fields`'s
    /// doc comment on how `kind` was built. Sends the matching
    /// `ResolveLocalCommit`/`ResolveRemoteCommit` and records `target` so
    /// `apply_commit_fetch_result` knows which dependency to fill in once
    /// the reply arrives.
    fn dependency_commit_auto_clicked(&mut self, target: DependencySlot, kind: AutoCommitKind) {
        let request = self.next_request_id();
        self.pending.insert(request, PendingKind::Generic);
        if let EditorState::NewRequirement(form) = &mut self.editor {
            form.pending_commit_fetches.insert(request, target);
            form.commit_fetch_error = None;
        } else {
            return;
        }
        let command = match kind {
            AutoCommitKind::Local(target) => Command::ResolveLocalCommit {
                target,
                kind: EntryKind::Requirement,
                request,
            },
            AutoCommitKind::Remote { url, path } => Command::ResolveRemoteCommit { url, path, request },
        };
        self.send_command(command);
    }

    /// Applies a `ResolveLocalCommit`/`ResolveRemoteCommit` reply to
    /// whichever dependency slot requested it. A no-op if the Requirement
    /// form has since closed, switched to a different entry, or the row
    /// itself was removed while the fetch was in flight — same "a stale
    /// reply is simply dropped" precedent as `apply_local_pool_change`.
    fn apply_commit_fetch_result(&mut self, request: RequestId, result: Result<String, String>) {
        let EditorState::NewRequirement(form) = &mut self.editor else {
            return;
        };
        let Some(target) = form.pending_commit_fetches.remove(&request) else {
            return;
        };
        match result {
            Ok(commit) => {
                let dep = match target {
                    DependencySlot::Existing(i) => form.dependencies.get_mut(i),
                    DependencySlot::New => Some(&mut form.new_dependency),
                };
                let Some(dep) = dep else {
                    return;
                };
                match dep {
                    DependencyDraft::LocalRequirement { commit: field, .. } => *field = commit,
                    DependencyDraft::Remote { commit: field, .. } => *field = commit,
                    DependencyDraft::Submodules => {}
                }
                // Only an already-added row is part of the form's real,
                // submitted content — same distinction the "Add
                // dependency" composer's own edits get in the render code
                // (see the comment above `render_dependency_kind_picker`'s
                // "Add dependency" call site).
                if let DependencySlot::Existing(_) = target {
                    form.edited = true;
                }
            }
            Err(message) => form.commit_fetch_error = Some(message),
        }
    }

    fn apply_local_pool_outcome(&mut self, request: RequestId, result: Result<(), String>) {
        let Some(op) = self.local_pool_ops.remove(&request) else {
            return;
        };
        match result {
            Ok(()) => {
                self.dirty = true;
                self.apply_local_pool_change(&op);
            }
            Err(message) => self.set_local_pool_error(op.kind, &op.target, message),
        }
    }

    /// Applies a successful add/remove directly to whichever form is
    /// currently open, in place — no round-trip through `GetEntryDetail`.
    /// A no-op if the editor has moved on to something else since this
    /// operation was sent (a different entry, or closed) — that reply is
    /// simply stale.
    fn apply_local_pool_change(&mut self, op: &LocalPoolOp) {
        match (&mut self.editor, op.kind) {
            (EditorState::NewRequirement(form), LocalPoolKind::RequirementAttachment)
                if form.editing_target.as_ref() == Some(&op.target) =>
            {
                apply_pool_op(&mut form.attachments, op);
                form.local_pool_error = None;
            }
            (EditorState::NewTest(form), LocalPoolKind::TestAttachment)
                if form.editing_target.as_ref() == Some(&op.target) =>
            {
                apply_pool_op(&mut form.attachments, op);
                form.local_pool_error = None;
            }
            (EditorState::NewTest(form), LocalPoolKind::TestTemplate)
                if form.editing_target.as_ref() == Some(&op.target) =>
            {
                apply_pool_op(&mut form.template_files, op);
                form.local_pool_error = None;
            }
            (EditorState::NewResult(form), LocalPoolKind::ResultAttachment)
                if form.editing_target.as_ref() == Some(&op.target) =>
            {
                apply_pool_op(&mut form.attachments, op);
                form.local_pool_error = None;
            }
            _ => {}
        }
    }

    fn set_local_pool_error(&mut self, kind: LocalPoolKind, target: &LogicalPath, message: String) {
        match (&mut self.editor, kind) {
            (EditorState::NewRequirement(form), LocalPoolKind::RequirementAttachment)
                if form.editing_target.as_ref() == Some(target) =>
            {
                form.local_pool_error = Some(message);
            }
            (EditorState::NewTest(form), LocalPoolKind::TestAttachment | LocalPoolKind::TestTemplate)
                if form.editing_target.as_ref() == Some(target) =>
            {
                form.local_pool_error = Some(message);
            }
            (EditorState::NewResult(form), LocalPoolKind::ResultAttachment)
                if form.editing_target.as_ref() == Some(target) =>
            {
                form.local_pool_error = Some(message);
            }
            _ => {}
        }
    }

    /// The currently-open form's target (`editing_target`) plus its kind
    /// — `None` for a create-mode form (nothing to view/edit toggle for
    /// something that doesn't exist yet) or when nothing is open at all.
    /// Shared by `editor_edit_clicked`/`editor_cancel_clicked`, the two
    /// places that need to turn "whichever form is open" back into a
    /// `(LogicalPath, EntryKind)` to hand to `navigate`.
    fn editing_target_and_kind(&self) -> Option<(LogicalPath, EntryKind)> {
        match &self.editor {
            EditorState::NewRequirement(f) => f.editing_target.clone().map(|t| (t, EntryKind::Requirement)),
            EditorState::NewTest(f) => f.editing_target.clone().map(|t| (t, EntryKind::Test)),
            EditorState::NewResult(f) => f.editing_target.clone().map(|t| (t, EntryKind::Result)),
            EditorState::NewModule(_) | EditorState::ExistingModule(_) | EditorState::None => None,
        }
    }

    /// The read-only viewer's "Edit" button — switches to the editable
    /// form for the same entry. A real navigation (`NavMode::Edit`), not
    /// a local flag flip: re-fetching keeps the edit form's starting
    /// point authoritative rather than trusting whatever's currently
    /// rendered, and — per the user's own request — registers with
    /// Back/Forward like any other navigation, so Back from the edit form
    /// returns to the viewer instead of skipping past it. A no-op if
    /// nothing with an existing target is open (there's no "Edit" button
    /// to click in that case anyway).
    fn editor_edit_clicked(&mut self) {
        // The module/project page's Edit is a local flag flip, not a
        // navigation: there's no `NavMode`/`nav_history` for module
        // selections (see `nav_history`'s own doc comment), and nothing
        // about the already-fetched `summary` goes stale by toggling
        // `read_only` — unlike a leaf's edit form, which re-fetches to
        // keep its starting point authoritative.
        if let EditorState::ExistingModule(form) = &mut self.editor {
            form.new_name = form.display_name.clone();
            form.read_only = false;
            form.edited = false;
            form.error = None;
            return;
        }
        if let Some((target, kind)) = self.editing_target_and_kind() {
            self.navigate(target, kind, NavMode::Edit);
        }
    }

    /// The edit form's Cancel button. For an existing entry
    /// (`editing_target: Some`), this is also a navigation — back to the
    /// same entry's read-only viewer (`NavMode::View`), re-fetched fresh
    /// so any unsaved typing is genuinely discarded rather than lingering
    /// in a "read-only" form that still shows the abandoned edit. A
    /// create-mode form (`editing_target: None`) has no viewer to return
    /// to, so it just closes, same as before this existed.
    fn editor_cancel_clicked(&mut self) {
        // Mirrors `editor_edit_clicked`'s module/project special case — a
        // local flip back to the viewer, discarding `new_name`; nothing
        // else in the page was mutable, so there's nothing to re-fetch.
        if let EditorState::ExistingModule(form) = &mut self.editor {
            form.read_only = true;
            form.edited = false;
            form.error = None;
            return;
        }
        match self.editing_target_and_kind() {
            Some((target, kind)) => self.navigate(target, kind, NavMode::View),
            None => self.editor = EditorState::None,
        }
    }

    /// The active form's Create button. Builds the `Command` from
    /// whichever form is open, marks it as this form's pending request
    /// (see `apply_create_result`), and sends it — same fire-and-poll
    /// shape as every other mutating action, not a blocking call.
    fn editor_create_clicked(&mut self) {
        let module = self.new_entry_module_path();
        let request = self.next_request_id();
        let command = match &mut self.editor {
            EditorState::NewRequirement(form) => {
                form.pending_request = Some(request);
                form.error = None;
                Some(form.build_command(module, request))
            }
            EditorState::NewTest(form) => {
                form.pending_request = Some(request);
                form.error = None;
                Some(form.build_command(module, request))
            }
            EditorState::NewResult(form) => {
                form.pending_request = Some(request);
                form.error = None;
                Some(form.build_command(module, request))
            }
            EditorState::NewModule(form) => {
                form.pending_request = Some(request);
                form.error = None;
                Some(form.build_command(module, request))
            }
            EditorState::ExistingModule(form) => {
                form.pending_request = Some(request);
                form.error = None;
                Some(form.build_command(request))
            }
            EditorState::None => None,
        };
        if let Some(command) = command {
            self.pending.insert(request, PendingKind::Generic);
            self.send_command(command);
        }
    }

    /// The edit form's Delete button — opens the confirmation dialog
    /// rather than deleting immediately, per the user's request that
    /// delete always prompts first. Reads the target straight out of
    /// whichever form is currently open in edit mode; a no-op for a
    /// create-mode form (nothing saved yet to delete) or the project
    /// root's own module page (`render_module_page` doesn't offer this
    /// button there in the first place, but this stays total rather than
    /// trusting the caller).
    fn editor_delete_clicked(&mut self) {
        let (target, label) = match &self.editor {
            EditorState::NewRequirement(form) => match form.editing_target.clone() {
                Some(target) => (DeleteTarget::Requirement(target), form.name.clone()),
                None => return,
            },
            EditorState::NewTest(form) => match form.editing_target.clone() {
                Some(target) => (DeleteTarget::Test(target), form.name.clone()),
                None => return,
            },
            EditorState::NewResult(form) => match form.editing_target.clone() {
                Some(target) => (DeleteTarget::Result(target), form.name.clone()),
                None => return,
            },
            EditorState::ExistingModule(form) if !form.path.is_empty() => {
                (DeleteTarget::Module(form.path.clone()), form.display_name.clone())
            }
            EditorState::ExistingModule(_) | EditorState::NewModule(_) | EditorState::None => return,
        };
        self.delete_confirm_dialog = Some(DeleteConfirmState {
            target,
            label,
            pending_request: None,
            error: None,
        });
    }

    /// "Delete" clicked inside the confirmation dialog — sends the
    /// `Command::Remove*` matching `DeleteConfirmState::target` and
    /// starts tracking the reply. Left `pending_request: None` (a no-op)
    /// if the dialog isn't open, which shouldn't happen since this is
    /// only ever reached from the dialog's own button.
    fn delete_confirmed(&mut self) {
        let Some(dialog) = &self.delete_confirm_dialog else {
            return;
        };
        let target = dialog.target.clone();
        let request = self.next_request_id();
        let command = match &target {
            DeleteTarget::Requirement(target) => Command::RemoveRequirement {
                target: target.clone(),
                request,
            },
            DeleteTarget::Test(target) => Command::RemoveTest {
                target: target.clone(),
                request,
            },
            DeleteTarget::Result(target) => Command::RemoveResult {
                target: target.clone(),
                request,
            },
            DeleteTarget::Module(target) => Command::RemoveModule {
                target: target.clone(),
                request,
            },
        };
        self.pending.insert(request, PendingKind::Generic);
        if let Some(dialog) = &mut self.delete_confirm_dialog {
            dialog.pending_request = Some(request);
            dialog.error = None;
        }
        self.send_command(command);
    }

    /// "Cancel" clicked in the confirmation dialog — just closes it.
    /// Nothing to undo since `delete_confirmed` (the only thing that
    /// mutates anything) hasn't run yet at that point.
    fn delete_cancelled(&mut self) {
        self.delete_confirm_dialog = None;
    }

    /// Routes a `RemoveRequirement`/`RemoveTest`/`RemoveResult`/
    /// `RemoveModule` outcome back to the delete-confirmation dialog that
    /// sent it — a reply for a dialog the user already cancelled is
    /// ignored, same "stale reply" handling as `apply_create_result`.
    /// Success marks the project dirty (the removal only reaches disk on
    /// the next Save — `disk::util::remove_stale_children` reconciles
    /// then, not immediately) and navigates to the deleted entry's parent
    /// module, since its own now-gone viewer has nothing left to show.
    /// Failure (`removed: false` — the entry was already gone) leaves the
    /// dialog open with an error instead of silently closing on a delete
    /// that didn't actually happen.
    fn apply_delete_result(&mut self, request: RequestId, removed: bool) {
        let is_pending = matches!(&self.delete_confirm_dialog, Some(d) if d.pending_request == Some(request));
        if !is_pending {
            return;
        }
        if removed {
            self.dirty = true;
            let target = self.delete_confirm_dialog.take().expect("just matched Some above").target;
            let parent = match target {
                DeleteTarget::Requirement(target) | DeleteTarget::Test(target) | DeleteTarget::Result(target) => {
                    target.modules
                }
                DeleteTarget::Module(target) => {
                    let len = target.len().saturating_sub(1);
                    target[..len].to_vec()
                }
            };
            self.select_module(parent);
        } else if let Some(dialog) = &mut self.delete_confirm_dialog {
            dialog.pending_request = None;
            dialog.error = Some("Could not delete — it may have already been removed.".to_string());
        }
    }

    /// Routes an `AddRequirement`/`AddTest`/`AddResult`/`AddModule`
    /// outcome back into whichever form is still open and waiting on it —
    /// a reply for a form the user already cancelled/closed is ignored,
    /// same "stale reply" handling as `select`'s `detail_request`. Success
    /// closes the form (creation is done); failure reports the error
    /// inline and leaves the form open so the user can fix and retry,
    /// rather than losing what they typed. Only ever reached for a
    /// create-mode form (`editing_target: None`) — that's the only way an
    /// `Add*` `Command` gets sent in the first place, see
    /// `RequirementFormState::build_command`.
    fn apply_create_result(&mut self, request: RequestId, result: Result<(), gui_core::AddChildError>) {
        let is_pending = match &self.editor {
            EditorState::NewRequirement(f) => f.pending_request == Some(request),
            EditorState::NewTest(f) => f.pending_request == Some(request),
            EditorState::NewResult(f) => f.pending_request == Some(request),
            EditorState::NewModule(f) => f.pending_request == Some(request),
            EditorState::ExistingModule(_) | EditorState::None => false,
        };
        if !is_pending {
            return;
        }

        match result {
            Ok(()) => {
                self.dirty = true;
                self.editor = EditorState::None;
            }
            Err(err) => {
                let message = err.to_string();
                match &mut self.editor {
                    EditorState::NewRequirement(f) => {
                        f.error = Some(message);
                        f.pending_request = None;
                    }
                    EditorState::NewTest(f) => {
                        f.error = Some(message);
                        f.pending_request = None;
                    }
                    EditorState::NewResult(f) => {
                        f.error = Some(message);
                        f.pending_request = None;
                    }
                    EditorState::NewModule(f) => {
                        f.error = Some(message);
                        f.pending_request = None;
                    }
                    EditorState::ExistingModule(_) | EditorState::None => {}
                }
            }
        }
    }

    /// The `Update*` counterpart to `apply_create_result` — same stale-
    /// reply guard, but success keeps the form open (just clears
    /// `pending_request`) instead of closing it: unlike a create, there's
    /// no "done, blank slate" moment for an edit — the form still shows
    /// the (now-saved) entry, which is the reasonable thing to keep
    /// looking at. Only ever reached for an edit-mode form
    /// (`editing_target: Some(_)`), the mirror image of
    /// `apply_create_result`'s note.
    fn apply_update_result(&mut self, request: RequestId, result: Result<(), gui_core::UpdateChildError>) {
        let is_pending = match &self.editor {
            EditorState::NewRequirement(f) => f.pending_request == Some(request),
            EditorState::NewTest(f) => f.pending_request == Some(request),
            EditorState::NewResult(f) => f.pending_request == Some(request),
            EditorState::NewModule(_) | EditorState::ExistingModule(_) | EditorState::None => false,
        };
        if !is_pending {
            return;
        }

        let succeeded = result.is_ok();
        if succeeded {
            self.dirty = true;
        }
        let message = result.err().map(|err| err.to_string());
        // `edited` only clears on success — a failed save leaves the
        // form's content still genuinely unsaved, so the "you have
        // unsaved changes" prompt (`editor_has_unsaved_edits`) must keep
        // applying to it.
        match &mut self.editor {
            EditorState::NewRequirement(f) => {
                f.error = message;
                f.pending_request = None;
                f.edited = !succeeded;
            }
            EditorState::NewTest(f) => {
                f.error = message;
                f.pending_request = None;
                f.edited = !succeeded;
            }
            EditorState::NewResult(f) => {
                f.error = message;
                f.pending_request = None;
                f.edited = !succeeded;
            }
            EditorState::NewModule(_) | EditorState::ExistingModule(_) | EditorState::None => {}
        }
    }

    /// Applies a `RefreshStaleTestReferences` reply — same stale-reply
    /// guard as `apply_update_result` (checked against `form.pending_request`),
    /// but its own method rather than folded into that shared one: this
    /// button only ever appears in the read-only viewer, so, unlike a
    /// real Save/Create, a success here is always safe to react to with a
    /// full `GetEntryDetail` re-fetch — there's no in-progress edit it
    /// could clobber. `gui-core` implicitly revalidates on success (see
    /// `Command::RefreshStaleTestReferences`'s own doc comment), so this
    /// re-fetch reflects the real, freshly-recomputed `met_status` rather
    /// than a blanket `Unvalidated`.
    fn apply_refresh_stale_test_references_result(
        &mut self,
        request: RequestId,
        result: Result<(), gui_core::RefreshStaleTestReferencesError>,
    ) {
        let target = {
            let EditorState::NewRequirement(form) = &mut self.editor else {
                return;
            };
            if form.pending_request != Some(request) {
                return;
            }
            form.pending_request = None;
            match result {
                Ok(()) => {
                    form.error = None;
                    form.editing_target.clone()
                }
                Err(err) => {
                    form.error = Some(err.to_string());
                    None
                }
            }
        };
        let Some(target) = target else {
            return;
        };
        self.dirty = true;
        let detail_request = self.next_request_id();
        self.pending.insert(detail_request, PendingKind::Generic);
        self.detail_request = Some(detail_request);
        self.send_command(Command::GetEntryDetail {
            target,
            kind: EntryKind::Requirement,
            request: detail_request,
        });
    }

    /// The Exit button / File -> Exit / window close handler. See
    /// README's "Exit" section, "Stage 1: prompt to save, bounded so it
    /// cannot hang."
    fn on_exit_clicked(&mut self) {
        self.exit_dialog = Some(if self.dirty {
            ExitDialogState::Asking
        } else {
            ExitDialogState::Ready
        });
    }

    fn on_exit_dialog_save_clicked(&mut self) {
        let request = self.next_request_id();
        self.send_command(Command::Save { request });
        self.exit_dialog = Some(ExitDialogState::Saving {
            request,
            deadline: Instant::now() + self.config.save_on_exit_timeout,
        });
    }

    fn on_exit_dialog_discard_clicked(&mut self) {
        self.exit_dialog = Some(ExitDialogState::Ready);
    }

    fn on_exit_dialog_cancel_clicked(&mut self) {
        self.exit_dialog = None;
    }

    fn on_exit_dialog_exit_anyway_clicked(&mut self) {
        self.exit_dialog = Some(ExitDialogState::Ready);
    }

    fn on_exit_dialog_keep_waiting_clicked(&mut self) {
        if let Some(ExitDialogState::TimedOut { request }) = self.exit_dialog {
            self.exit_dialog = Some(ExitDialogState::Saving {
                request,
                deadline: Instant::now() + self.config.save_on_exit_timeout,
            });
        }
    }

    /// Called once per frame. Advances `Saving` -> `TimedOut` once the
    /// deadline passes — the only place time is consulted in this whole
    /// state machine, so it's the only thing a test needs to control to
    /// exercise the timeout path deterministically (pass a `now` past the
    /// deadline rather than actually sleeping).
    fn tick_exit_dialog(&mut self, now: Instant) {
        if let Some(ExitDialogState::Saving { request, deadline }) = self.exit_dialog
            && now >= deadline
        {
            self.exit_dialog = Some(ExitDialogState::TimedOut { request });
        }
    }

    /// `true` at most once per resolution of the dialog — consumes
    /// `Ready` so Stage 2 (`Command::Shutdown` + close) runs exactly once
    /// per Exit, not every frame afterward.
    fn take_ready_to_exit(&mut self) -> bool {
        if matches!(self.exit_dialog, Some(ExitDialogState::Ready)) {
            self.exit_dialog = None;
            true
        } else {
            false
        }
    }
}

/// Applies one confirmed local-pool add/remove to a form's path list —
/// shared by every `(EditorState, LocalPoolKind)` arm in
/// `GuiApp::apply_local_pool_change`, since "add = push (sorted, no
/// duplicate)" / "remove = filter out" is identical regardless of which
/// list it's operating on.
fn apply_pool_op(paths: &mut Vec<PathBuf>, op: &LocalPoolOp) {
    if op.adding {
        if !paths.contains(&op.path) {
            paths.push(op.path.clone());
            paths.sort();
        }
    } else {
        paths.retain(|p| p != &op.path);
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_events();
        #[cfg(all(feature = "debug-panel", debug_assertions))]
        self.flush_stalled_tx();

        // `set_zoom_factor` is a no-op when the value hasn't actually
        // changed (checked internally), so it's cheap enough to just
        // call unconditionally every frame rather than tracking "did the
        // zoom change this frame" separately.
        ui.ctx().set_zoom_factor(self.config.zoom_percent as f32 / 100.0);
        // Same "cheap enough to set unconditionally" reasoning as
        // `set_zoom_factor` above — `set_theme` just writes
        // `Options::theme_preference`, no per-call cost worth tracking
        // "did this actually change" around.
        ui.ctx().set_theme(self.config.theme);

        // The window's own close control (OS "X" / Alt-F4 / Cmd-Q) — route
        // it through the same Stage 1/2 exit flow as the Exit button/menu
        // item rather than letting eframe close the window on its own,
        // which would skip the unsaved-changes prompt entirely.
        //
        // Once `shutdown_sent`, never touch this again — see its own doc
        // comment on why: Stage 2 below re-requests the close via
        // `ViewportCommand::Close` when nothing already has one in
        // flight, and that resurfaces as `close_requested()` on a later
        // pass exactly like a fresh OS click would, so re-entering this
        // block for it would cancel that close and (having already
        // resolved `exit_dialog` to `None`) immediately reopen the
        // dialog — forever, since the newly reopened dialog's own
        // eventual `Ready` would just re-request the close and repeat
        // the cycle. Confirmed as a real regression: a not-dirty
        // project's window-close button hung in exactly this loop before
        // `shutdown_sent` was added.
        let close_requested = ui.ctx().input(|i| i.viewport().close_requested());
        if close_requested && !self.shutdown_sent {
            if self.exit_dialog.is_none() {
                self.on_exit_clicked(); // Asking (dirty) or Ready (not)
            }
            if !matches!(self.exit_dialog, Some(ExitDialogState::Ready)) {
                // Still needs a prompt, or one's already up (including a
                // repeat click on the window control while it's showing)
                // — hold the close until it resolves. When `on_exit_clicked`
                // just set `Ready` above (nothing dirty), deliberately
                // *don't* cancel: Stage 2 below sees `close_requested`
                // still true this same pass and lets it complete on its
                // own rather than roundtripping through another manufactured
                // `ViewportCommand::Close`.
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }

        self.tick_exit_dialog(Instant::now());

        if self.take_ready_to_exit() {
            // Stage 2: unconditional immediate close — see README's
            // "Exit" section. Fire-and-forget; nothing here waits on
            // gui-core. Deliberately `self.core.send` directly, not
            // `send_command` — the debug panel's Tx stall/failure
            // injection must never be able to hold up or drop `Shutdown`
            // itself, or it would defeat this crate's "the exit button
            // always works, nothing can prevent it" guarantee.
            self.core.send(Command::Shutdown);
            self.shutdown_sent = true;
            if !close_requested {
                // The Exit button/menu path: nothing's already mid-close,
                // so this has to ask for one explicitly. The window-close
                // path left its own `close_requested` uncancelled above
                // (or never canceled it in the first place), so it's
                // already on track to complete without this.
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // See view.rs — menu bar/toolbar/status bar (top-to-bottom
        // TopBottomPanel-equivalents) first, then the left tree pane, then
        // whatever's left is the center pane. Order matters: each Panel
        // claims space from what's left, so panels have to run before the
        // CentralPanel that fills the remainder.
        self.render_menu_bar(ui);
        self.render_toolbar(ui);
        self.render_status_bar(ui);
        self.render_left_pane(ui);
        #[cfg(all(feature = "debug-panel", debug_assertions))]
        self.render_debug_panel(ui);
        self.render_center_pane(ui);
        self.render_new_project_dialog(ui);
        self.render_unsaved_changes_dialog(ui);
        self.render_unsaved_form_dialog(ui);
        self.render_validate_before_save_dialog(ui);
        self.render_delete_confirm_dialog(ui);
        self.render_load_error_dialog(ui);
        self.render_attachments_dialog(ui);
        self.render_path_picker_dialog(ui);
        self.render_exit_dialog(ui);
        #[cfg(all(feature = "debug-panel", debug_assertions))]
        self.render_debug_confirm_dialog(ui);

        // Keep polling try_recv_event promptly even with no user input —
        // see README's "Never block the render thread".
        ui.ctx().request_repaint();
    }
}

/// Every requirement (or test) in the currently-loaded tree, as
/// `LogicalPath`s — the path-picker modal's own option list (`view.rs`'s
/// `render_path_picker_dialog`). Walks `tree.root.children` directly (not
/// `render_tree_node`'s recursion), for the same reason `render_left_pane`
/// renders the root specially: the root `TreeNode`'s own `name` is a
/// display label, not a real module-path segment, and must never be
/// pushed into a child's path.
pub(crate) fn flatten_leaf_paths(tree: &TreeSnapshot, kind: EntryKind) -> Vec<LogicalPath> {
    let mut out = Vec::new();
    collect_leaf_paths(&tree.root.children, kind, &[], &mut out);
    out
}

fn collect_leaf_paths(children: &[TreeNode], kind: EntryKind, module_path: &[EntryName], out: &mut Vec<LogicalPath>) {
    for child in children {
        if child.kind == EntryKind::Module {
            let mut child_path = module_path.to_vec();
            child_path.push(child.name.clone());
            collect_leaf_paths(&child.children, kind, &child_path, out);
        } else if child.kind == kind {
            out.push(LogicalPath {
                modules: module_path.to_vec(),
                name: child.name.clone(),
            });
        }
    }
}

/// The absolute (project-root-relative, leading-`/`) `disk::ReferencePath`
/// string for `target` — matches `logical::path::parse_reference_path`'s
/// expected `/[modules/<sub>/]*<kind_segment>/<name>` format exactly (see
/// that function's own parsing logic in `crates/logical/src/path.rs`),
/// which is what `ResultDraft::requirement_path`/`test_path` are parsed
/// against at `validate()` time. `kind_segment` is `"requirements"` or
/// `"tests"` — the on-disk directory name, not `EntryKind`'s Rust-side
/// spelling.
/// The display name for a module path, read straight out of `tree` rather
/// than a separate round trip — the tree already carries every module's
/// name (it's a `TreeNode` per module), so there's nothing `gui-core` needs
/// to be asked for here. Falls back to the path's own last segment if the
/// tree hasn't loaded yet or the path doesn't (yet) resolve — same "never
/// panic on a momentarily-stale view" spirit as the rest of this module.
/// A free function (not a `&self` method) so it can be called from
/// `apply_event`'s `Event::TreeChanged` arm while `self.editor` is
/// separately borrowed mutably — see that arm's own comment.
fn module_display_name(tree: Option<&TreeSnapshot>, path: &[EntryName]) -> String {
    let Some(tree) = tree else {
        return path.last().map(|name| name.as_str().to_string()).unwrap_or_default();
    };
    let mut node = &tree.root;
    for segment in path {
        match node.children.iter().find(|child| child.kind == EntryKind::Module && &child.name == segment) {
            Some(child) => node = child,
            None => return segment.as_str().to_string(),
        }
    }
    node.name.as_str().to_string()
}

pub(crate) fn absolute_reference_path(target: &LogicalPath, kind_segment: &str) -> String {
    let mut path = String::from("/");
    for module in &target.modules {
        path.push_str("modules/");
        path.push_str(module.as_str());
        path.push('/');
    }
    path.push_str(kind_segment);
    path.push('/');
    path.push_str(target.name.as_str());
    path
}

/// The on-disk directory name for a leaf `kind` — `"requirements"`/
/// `"tests"`/`"results"`, matching `disk`'s own project layout (see
/// `absolute_reference_path`'s doc comment). `EntryKind::Module` has no
/// leaf path of its own, so no sensible mapping — every caller
/// (`view.rs`'s `node_matches_filter`/`render_path_picker_dialog`,
/// `path_picker_dialog_selected` above) only ever reaches this with a real
/// leaf kind.
pub(crate) fn leaf_kind_segment(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Requirement => "requirements",
        EntryKind::Test => "tests",
        EntryKind::Result => "results",
        EntryKind::Module => unreachable!("a module has no leaf path of its own"),
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use super::*;

    fn test_app() -> GuiApp {
        // `/dev/null`: a harmless place for a stray `persist_config`/
        // `record_recent_project` write to land — no test here cares
        // whether a zoom change or a recent-project record actually
        // reached disk (that's `config::test`'s/`recent::test`'s job).
        GuiApp::new(
            CoreHandle::start(),
            GuiConfig::default(),
            PathBuf::from("/dev/null"),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    }

    #[test]
    fn exit_with_no_unsaved_changes_goes_straight_to_ready() {
        let mut app = test_app();
        assert!(!app.dirty);

        app.on_exit_clicked();

        assert_eq!(app.exit_dialog, Some(ExitDialogState::Ready));
        assert!(app.take_ready_to_exit());
        assert_eq!(app.exit_dialog, None);
        // Only fires once.
        assert!(!app.take_ready_to_exit());
    }

    #[test]
    fn exit_with_unsaved_changes_asks_first() {
        let mut app = test_app();
        app.dirty = true;

        app.on_exit_clicked();

        assert_eq!(app.exit_dialog, Some(ExitDialogState::Asking));
        assert!(!app.take_ready_to_exit());
    }

    #[test]
    fn cancel_dismisses_the_dialog_without_exiting() {
        let mut app = test_app();
        app.dirty = true;
        app.on_exit_clicked();

        app.on_exit_dialog_cancel_clicked();

        assert_eq!(app.exit_dialog, None);
        assert!(!app.take_ready_to_exit());
    }

    #[test]
    fn discard_proceeds_to_exit_without_saving() {
        let mut app = test_app();
        app.dirty = true;
        app.on_exit_clicked();

        app.on_exit_dialog_discard_clicked();

        assert!(app.take_ready_to_exit());
        // dirty is untouched by Discard — nothing was actually saved.
        assert!(app.dirty);
    }

    #[test]
    fn save_then_a_matching_completion_proceeds_to_exit_and_clears_dirty() {
        let mut app = test_app();
        app.dirty = true;
        app.on_exit_clicked();
        app.on_exit_dialog_save_clicked();

        let request = match app.exit_dialog {
            Some(ExitDialogState::Saving { request, .. }) => request,
            other => panic!("expected Saving, got {other:?}"),
        };

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Save(Ok(())),
        });

        assert!(!app.dirty);
        assert!(app.take_ready_to_exit());
    }

    #[test]
    fn save_then_a_failed_completion_still_proceeds_once_exit_anyway_is_chosen() {
        let mut app = test_app();
        app.dirty = true;
        app.on_exit_clicked();
        app.on_exit_dialog_save_clicked();
        let request = match app.exit_dialog {
            Some(ExitDialogState::Saving { request, .. }) => request,
            other => panic!("expected Saving, got {other:?}"),
        };

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Save(Err(gui_core::SaveError::NotValidated)),
        });

        // A failed save still resolves the wait — see apply_outcome's
        // comment — the user gets to decide what happens next via the
        // (not-yet-rendered) dialog, not gui-ui deciding for them.
        assert!(app.take_ready_to_exit());
        // dirty stays true: the save didn't actually succeed.
        assert!(app.dirty);
    }

    #[test]
    fn a_completion_for_a_different_request_does_not_resolve_the_dialog() {
        let mut app = test_app();
        app.dirty = true;
        app.on_exit_clicked();
        app.on_exit_dialog_save_clicked();
        let saving_request = match app.exit_dialog {
            Some(ExitDialogState::Saving { request, .. }) => request,
            other => panic!("expected Saving, got {other:?}"),
        };

        app.apply_event(Event::Completed {
            request: saving_request + 1000, // an unrelated, older/other request
            outcome: Outcome::AddRequirement(Ok(())),
        });

        assert!(matches!(app.exit_dialog, Some(ExitDialogState::Saving { .. })));
    }

    #[test]
    fn tick_advances_saving_to_timed_out_once_the_deadline_passes() {
        let mut app = test_app();
        app.config.save_on_exit_timeout = Duration::from_secs(1);
        app.dirty = true;
        app.on_exit_clicked();
        app.on_exit_dialog_save_clicked();
        let request = match app.exit_dialog {
            Some(ExitDialogState::Saving { request, .. }) => request,
            other => panic!("expected Saving, got {other:?}"),
        };

        // A `now` before the deadline: no transition yet.
        app.tick_exit_dialog(Instant::now());
        assert!(matches!(app.exit_dialog, Some(ExitDialogState::Saving { .. })));

        // A `now` past the deadline: transitions deterministically, no
        // real sleep needed.
        app.tick_exit_dialog(Instant::now() + Duration::from_secs(2));
        assert_eq!(app.exit_dialog, Some(ExitDialogState::TimedOut { request }));
    }

    #[test]
    fn keep_waiting_returns_to_saving_with_a_fresh_deadline() {
        let mut app = test_app();
        app.config.save_on_exit_timeout = Duration::from_secs(1);
        app.dirty = true;
        app.on_exit_clicked();
        app.on_exit_dialog_save_clicked();
        app.tick_exit_dialog(Instant::now() + Duration::from_secs(2));
        assert!(matches!(app.exit_dialog, Some(ExitDialogState::TimedOut { .. })));

        app.on_exit_dialog_keep_waiting_clicked();

        assert!(matches!(app.exit_dialog, Some(ExitDialogState::Saving { .. })));
        // Fresh deadline: an immediate tick doesn't re-time-out.
        app.tick_exit_dialog(Instant::now());
        assert!(matches!(app.exit_dialog, Some(ExitDialogState::Saving { .. })));
    }

    #[test]
    fn exit_anyway_from_timed_out_proceeds_to_exit() {
        let mut app = test_app();
        app.config.save_on_exit_timeout = Duration::from_secs(1);
        app.dirty = true;
        app.on_exit_clicked();
        app.on_exit_dialog_save_clicked();
        app.tick_exit_dialog(Instant::now() + Duration::from_secs(2));
        assert!(matches!(app.exit_dialog, Some(ExitDialogState::TimedOut { .. })));

        app.on_exit_dialog_exit_anyway_clicked();

        assert!(app.take_ready_to_exit());
    }

    #[test]
    fn tree_changed_replaces_the_snapshot_wholesale() {
        let mut app = test_app();
        assert!(app.tree.is_none());

        let snapshot = TreeSnapshot {
            root: gui_core::TreeNode {
                name: disk_entry_name("Project"),
                kind: EntryKind::Module,
                status: gui_core::EntryStatus::Unvalidated,
                children: Vec::new(),
            },
            can_undo: false,
            can_redo: false,
        };
        app.apply_event(Event::TreeChanged(snapshot));

        assert!(app.tree.is_some());
    }

    fn disk_entry_name(name: &str) -> gui_core::EntryName {
        gui_core::EntryName(name.to_string())
    }

    #[test]
    fn select_sets_selection_and_closes_whatever_form_was_open() {
        let mut app = test_app();
        app.new_module_clicked();
        assert!(matches!(app.editor, EditorState::NewModule(_)));

        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);

        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("definition"))));
        assert!(matches!(app.editor, EditorState::None));
        assert!(app.detail_request.is_some());
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn a_fresh_app_cannot_go_back_or_forward() {
        let app = test_app();
        assert!(!app.can_go_back());
        assert!(!app.can_go_forward());
    }

    #[test]
    fn a_single_selection_still_cannot_go_back_or_forward() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        assert!(!app.can_go_back());
        assert!(!app.can_go_forward());
    }

    #[test]
    fn back_then_forward_round_trips_two_selections() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        app.select(LogicalPath::root(disk_entry_name("second")), EntryKind::Requirement);
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("second"))));
        assert!(app.can_go_back());
        assert!(!app.can_go_forward());

        app.back_clicked();
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("first"))));
        assert!(!app.can_go_back());
        assert!(app.can_go_forward());

        app.forward_clicked();
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("second"))));
        assert!(app.can_go_back());
        assert!(!app.can_go_forward());
    }

    #[test]
    fn back_clicked_with_nothing_to_go_back_to_does_nothing() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        let pending_before = app.pending.len();

        app.back_clicked();

        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("first"))));
        assert_eq!(app.pending.len(), pending_before);
    }

    #[test]
    fn forward_clicked_with_nothing_to_go_forward_to_does_nothing() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        let pending_before = app.pending.len();

        app.forward_clicked();

        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("first"))));
        assert_eq!(app.pending.len(), pending_before);
    }

    #[test]
    fn a_new_selection_after_going_back_discards_forward_history() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        app.select(LogicalPath::root(disk_entry_name("second")), EntryKind::Requirement);
        app.back_clicked();
        assert!(app.can_go_forward());

        // A genuinely new selection, not a `forward_clicked` — this
        // should truncate the "second" entry out of history entirely,
        // same as a browser dropping forward history on a fresh
        // navigation.
        app.select(LogicalPath::root(disk_entry_name("third")), EntryKind::Requirement);

        assert!(!app.can_go_forward());
        app.back_clicked();
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("first"))));
    }

    /// The bug this session's whole nav_history/`NavTarget` change fixes:
    /// before it, `select_module` never touched `nav_history` at all, so
    /// Back from a module page landed on whatever was selected *before*
    /// the leaf actually being viewed, silently skipping it.
    #[test]
    fn back_after_selecting_a_module_between_two_leaves_lands_on_the_middle_leaf() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        app.select(LogicalPath::root(disk_entry_name("second")), EntryKind::Requirement);
        app.select_module(vec![disk_entry_name("m")]);
        assert!(matches!(app.editor, EditorState::ExistingModule(_)));

        app.back_clicked();

        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("second"))));
        assert!(matches!(app.editor, EditorState::None));
    }

    #[test]
    fn selecting_a_module_after_going_back_discards_forward_history() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        app.select(LogicalPath::root(disk_entry_name("second")), EntryKind::Requirement);
        app.back_clicked();
        assert!(app.can_go_forward());

        app.select_module(vec![disk_entry_name("m")]);

        assert!(!app.can_go_forward());
        app.back_clicked();
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("first"))));
    }

    #[test]
    fn back_and_forward_walk_through_a_mixed_leaf_and_module_history() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("a")), EntryKind::Requirement);
        app.select_module(vec![disk_entry_name("m1")]);
        app.select(LogicalPath::root(disk_entry_name("b")), EntryKind::Requirement);
        app.select_module(Vec::new()); // the project root

        // history: [a, m1, b, root], position 3 (root)
        assert!(matches!(app.editor, EditorState::ExistingModule(_)));
        assert_eq!(app.selected_module, Vec::<gui_core::EntryName>::new());

        app.back_clicked(); // -> b
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("b"))));

        app.back_clicked(); // -> m1
        assert_eq!(app.selected_module, vec![disk_entry_name("m1")]);
        assert!(matches!(app.editor, EditorState::ExistingModule(_)));

        app.back_clicked(); // -> a
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("a"))));
        assert!(!app.can_go_back());

        app.forward_clicked(); // -> m1
        assert_eq!(app.selected_module, vec![disk_entry_name("m1")]);

        app.forward_clicked(); // -> b
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("b"))));

        app.forward_clicked(); // -> root
        assert_eq!(app.selected_module, Vec::<gui_core::EntryName>::new());
        assert!(!app.can_go_forward());
    }

    // --- Randomized/model-based nav_history fuzz test ---------------------
    //
    // An independent oracle for `nav_history`'s browser back/forward
    // semantics, built from the feature's own spec (a fresh navigation
    // truncates any forward history and becomes the new current stop;
    // Back/Forward just move `position` through the existing list) rather
    // than from reading `NavTarget`/`select_module`/`navigate` themselves —
    // otherwise this would just be restating the implementation back at
    // itself. Runs a long pseudorandom (but fixed-seed, reproducible)
    // sequence of real `GuiApp` actions against a tiny fake universe of
    // leaves/modules and checks the live app state against the model after
    // every step.

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Stop {
        Leaf { name: &'static str, mode: NavMode },
        /// Index into `FAKE_MODULES`; `0` is the project root.
        Module(usize),
    }

    struct Model {
        stops: Vec<Stop>,
        position: usize,
    }

    impl Model {
        fn new(first: Stop) -> Self {
            Model {
                stops: vec![first],
                position: 0,
            }
        }

        fn current(&self) -> Stop {
            self.stops[self.position]
        }

        /// A fresh navigation: truncate-and-push, same rule `navigate`/
        /// `select_module` both follow.
        fn navigate(&mut self, stop: Stop) {
            self.stops.truncate(self.position + 1);
            self.stops.push(stop);
            self.position = self.stops.len() - 1;
        }

        fn can_go_back(&self) -> bool {
            self.position > 0
        }

        fn can_go_forward(&self) -> bool {
            self.position + 1 < self.stops.len()
        }

        fn back(&mut self) {
            if self.can_go_back() {
                self.position -= 1;
            }
        }

        fn forward(&mut self) {
            if self.can_go_forward() {
                self.position += 1;
            }
        }
    }

    const FAKE_LEAVES: [&str; 3] = ["a", "b", "c"];
    /// `&[]` is the project root.
    const FAKE_MODULES: [&[&str]; 3] = [&[], &["m1"], &["m2"]];

    fn fake_module_path(index: usize) -> Vec<gui_core::EntryName> {
        FAKE_MODULES[index].iter().map(|s| disk_entry_name(s)).collect()
    }

    #[derive(Debug, Clone, Copy)]
    enum Action {
        SelectLeaf(usize),
        SelectModule(usize),
        EditClick,
        TriggerValidate,
        Back,
        Forward,
    }

    /// Completes whichever of `detail_request`/`module_summary_request`/
    /// `met_status_request` is currently outstanding with a canned reply,
    /// looping until none remain — generalizes
    /// `complete_definition_requirement_detail` to any fake leaf/module and
    /// to the extra requests `EditClick`/`TriggerValidate` can also
    /// trigger. "Outstanding" is judged via `app.pending` (cleared by
    /// `apply_event` for any completed request), not by the `*_request`
    /// fields going back to `None` — those are only ever overwritten by a
    /// fresher request, per their own doc comments (they exist to reject
    /// stale replies by id, not to signal "still waiting"). Bounded so a
    /// genuine bug (a request that never gets cleared) fails the test
    /// loudly instead of hanging it.
    fn settle(app: &mut GuiApp) {
        for _ in 0..10 {
            if let Some(request) = app.detail_request
                && app.pending.contains_key(&request)
            {
                app.apply_event(Event::Completed {
                    request,
                    outcome: Outcome::EntryDetail(Some(gui_core::EntryDetail::Requirement {
                        title: "Fake".to_string(),
                        requirement_text: String::new(),
                        requirement_guidance: None,
                        test_guidance: None,
                        dependencies: Vec::new(),
                        attachments: Vec::new(),
                        met_status: gui_core::RequirementMetStatus::Unvalidated,
                        original: Box::new(gui_core::RequirementDraft::new("Fake")),
                    })),
                });
                continue;
            }
            if let Some(request) = app.module_summary_request
                && app.pending.contains_key(&request)
            {
                app.apply_event(Event::Completed {
                    request,
                    outcome: Outcome::ModuleSummary(Some(gui_core::ModuleSummary::default())),
                });
                continue;
            }
            if let Some(request) = app.met_status_request
                && app.pending.contains_key(&request)
            {
                app.apply_event(Event::Completed {
                    request,
                    outcome: Outcome::RequirementMetStatus(gui_core::RequirementMetStatus::Unvalidated),
                });
                continue;
            }
            return;
        }
        panic!("settle: a request never drained after 10 rounds — likely a stale-reply bug");
    }

    /// Asserts the live `app` matches exactly what `model.current()` says
    /// should be on screen — the fuzz test's whole point, run after every
    /// single action.
    fn assert_matches_model(app: &GuiApp, model: &Model) {
        assert_eq!(app.can_go_back(), model.can_go_back());
        assert_eq!(app.can_go_forward(), model.can_go_forward());

        match model.current() {
            Stop::Leaf { name, mode } => {
                assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name(name))));
                let EditorState::NewRequirement(form) = &app.editor else {
                    panic!("expected NewRequirement, got {:?}", app.editor);
                };
                assert_eq!(form.editing_target, Some(LogicalPath::root(disk_entry_name(name))));
                assert_eq!(form.read_only, mode == NavMode::View);
            }
            Stop::Module(index) => {
                assert_eq!(app.selection, None);
                assert_eq!(app.selected_module, fake_module_path(index));
                let EditorState::ExistingModule(form) = &app.editor else {
                    panic!("expected ExistingModule, got {:?}", app.editor);
                };
                assert_eq!(form.path, fake_module_path(index));
            }
        }
    }

    fn run_nav_fuzz(seed: u64, actions: usize) {
        use rand::{Rng, SeedableRng, rngs::StdRng};

        let mut rng = StdRng::seed_from_u64(seed);
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name(FAKE_LEAVES[0])), EntryKind::Requirement);
        settle(&mut app);
        let mut model = Model::new(Stop::Leaf {
            name: FAKE_LEAVES[0],
            mode: NavMode::View,
        });
        assert_matches_model(&app, &model);

        for _ in 0..actions {
            let action = match rng.random_range(0..100u32) {
                0..=19 => Action::SelectLeaf(rng.random_range(0..FAKE_LEAVES.len())),
                20..=29 => Action::SelectModule(rng.random_range(0..FAKE_MODULES.len())),
                30..=39 => Action::EditClick,
                40..=49 => Action::TriggerValidate,
                50..=74 => Action::Back,
                _ => Action::Forward,
            };

            match action {
                Action::SelectLeaf(i) => {
                    let name = FAKE_LEAVES[i];
                    app.select(LogicalPath::root(disk_entry_name(name)), EntryKind::Requirement);
                    model.navigate(Stop::Leaf { name, mode: NavMode::View });
                }
                Action::SelectModule(i) => {
                    app.select_module(fake_module_path(i));
                    model.navigate(Stop::Module(i));
                }
                Action::EditClick => {
                    // Mirrors the real "Edit" button only being on screen
                    // for a read-only leaf viewer in the first place — the
                    // no-op cases (a module page, or an already-editable
                    // form) are already covered by
                    // `editor_edit_clicked_does_nothing_for_a_create_mode_form`
                    // and aren't what this test is about.
                    if let Stop::Leaf { name, mode: NavMode::View } = model.current() {
                        app.editor_edit_clicked();
                        model.navigate(Stop::Leaf { name, mode: NavMode::Edit });
                    }
                }
                Action::TriggerValidate => {
                    // Doesn't change what's the "current" nav stop, just
                    // proves it doesn't corrupt navigation while refreshing
                    // whatever's open.
                    app.apply_event(Event::Completed {
                        request: 999,
                        outcome: Outcome::Validate(Ok(())),
                    });
                }
                Action::Back => app.back_clicked(),
                Action::Forward => app.forward_clicked(),
            }
            if matches!(action, Action::Back | Action::Forward) {
                match action {
                    Action::Back => model.back(),
                    Action::Forward => model.forward(),
                    _ => unreachable!(),
                }
            }

            settle(&mut app);
            assert_matches_model(&app, &model);
        }
    }

    #[test]
    fn nav_history_matches_an_independent_model_across_hundreds_of_random_actions_seed_1() {
        run_nav_fuzz(1, 400);
    }

    #[test]
    fn nav_history_matches_an_independent_model_across_hundreds_of_random_actions_seed_2() {
        run_nav_fuzz(0xC0FFEE, 400);
    }

    #[test]
    fn a_matching_requirement_detail_reply_opens_it_pre_filled_and_read_only() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);
        let request = app.detail_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::EntryDetail(Some(gui_core::EntryDetail::Requirement {
                title: "Definition".to_string(),
                requirement_text: "The system shall...".to_string(),
                requirement_guidance: None,
                test_guidance: None,
                dependencies: Vec::new(),
                attachments: Vec::new(),
                met_status: gui_core::RequirementMetStatus::Unvalidated,
                original: Box::new(gui_core::RequirementDraft::new("Definition")),
            })),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            panic!("expected NewRequirement, got {:?}", app.editor);
        };
        assert_eq!(form.title, "Definition");
        assert_eq!(form.requirement_text, "The system shall...");
        assert_eq!(form.name, "definition");
        assert_eq!(form.editing_target, Some(LogicalPath::root(disk_entry_name("definition"))));
        assert!(form.pending_request.is_none());
        // A plain tree click (`select`, `NavMode::View`) lands on the
        // read-only viewer, not straight into the editable form — see
        // `editor_edit_clicked`'s own test below for the switch.
        assert!(form.read_only);
    }

    #[test]
    fn a_requirement_detail_replys_dependencies_populate_the_form() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);
        let request = app.detail_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::EntryDetail(Some(gui_core::EntryDetail::Requirement {
                title: "Definition".to_string(),
                requirement_text: String::new(),
                requirement_guidance: None,
                test_guidance: None,
                dependencies: vec![
                    gui_core::DependencyReferenceKind::RequirementReferenceV1(gui_core::LocalGitReference {
                        path: gui_core::ReferencePath("/requirements/discovery".to_string()),
                        commit: "abc123".to_string(),
                    }),
                    gui_core::DependencyReferenceKind::Submodules,
                ],
                attachments: Vec::new(),
                met_status: gui_core::RequirementMetStatus::Unvalidated,
                original: Box::new(gui_core::RequirementDraft::new("Definition")),
            })),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            panic!("expected NewRequirement, got {:?}", app.editor);
        };
        assert_eq!(form.dependencies.len(), 2);
        assert_eq!(
            form.dependencies[0],
            DependencyDraft::LocalRequirement {
                path: "/requirements/discovery".to_string(),
                commit: "abc123".to_string(),
            }
        );
        assert_eq!(form.dependencies[1], DependencyDraft::Submodules);
    }

    #[test]
    fn editor_edit_clicked_switches_the_viewer_to_the_editable_form() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);
        complete_definition_requirement_detail(&mut app);
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.read_only);

        app.editor_edit_clicked();
        complete_definition_requirement_detail(&mut app);

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(!form.read_only);
        assert_eq!(form.editing_target, Some(LogicalPath::root(disk_entry_name("definition"))));
    }

    #[test]
    fn editor_edit_clicked_registers_as_a_navigation_back_returns_to_the_viewer() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);
        complete_definition_requirement_detail(&mut app);
        assert!(!app.can_go_back());

        app.editor_edit_clicked();
        complete_definition_requirement_detail(&mut app);
        assert!(!matches!(&app.editor, EditorState::NewRequirement(f) if f.read_only));
        // Per the user's own request: switching to the edit form counts
        // as a navigation, so Back becomes available and returns to the
        // viewer rather than skipping past it to whatever was selected
        // before "definition" was ever clicked.
        assert!(app.can_go_back());

        app.back_clicked();
        complete_definition_requirement_detail(&mut app);
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.read_only);
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("definition"))));
    }

    #[test]
    fn editor_edit_clicked_does_nothing_for_a_create_mode_form() {
        let mut app = test_app();
        app.new_requirement_clicked();
        let pending_before = app.pending.len();

        app.editor_edit_clicked();

        assert!(matches!(app.editor, EditorState::NewRequirement(_)));
        assert_eq!(app.pending.len(), pending_before);
    }

    #[test]
    fn cancel_from_an_existing_entrys_edit_form_returns_to_its_viewer() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        // An unsaved edit, typed but never submitted.
        form.title = "Abandoned title".to_string();

        app.editor_cancel_clicked();
        // Cancel discards the unsaved edit by re-fetching, same as any
        // other navigation — the viewer must show the real, saved title,
        // not the abandoned one.
        complete_definition_requirement_detail(&mut app);

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.read_only);
        assert_eq!(form.title, "Definition");
    }

    #[test]
    fn editor_has_unsaved_edits_is_false_right_after_entering_the_edit_form() {
        let app = app_editing_a_requirement();
        assert!(!app.editor_has_unsaved_edits());
    }

    #[test]
    fn editor_has_unsaved_edits_is_true_once_a_field_is_marked_edited() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.edited = true;
        assert!(app.editor_has_unsaved_edits());
    }

    #[test]
    fn editor_has_unsaved_edits_is_false_for_the_read_only_viewer() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);
        complete_definition_requirement_detail(&mut app);
        // A plain tree click lands read-only — nothing editable to have
        // set `edited` in the first place.
        assert!(!app.editor_has_unsaved_edits());
    }

    #[test]
    fn a_successful_update_clears_the_edited_flag() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.edited = true;

        app.editor_create_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::UpdateRequirement(Ok(())),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(!form.edited);
    }

    #[test]
    fn a_failed_update_leaves_the_edited_flag_set() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.edited = true;

        app.editor_create_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::UpdateRequirement(Err(gui_core::UpdateChildError::NotFound)),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        // Still genuinely unsaved — the update never actually landed.
        assert!(form.edited);
    }

    #[test]
    fn unsaved_form_dialog_opened_sets_the_pending_navigation() {
        let mut app = test_app();
        app.unsaved_form_dialog_opened(PendingNavigation::Back);
        assert_eq!(app.unsaved_form_dialog, Some(PendingNavigation::Back));
    }

    #[test]
    fn unsaved_form_dialog_cancelled_clears_it_without_navigating() {
        let mut app = app_editing_a_requirement();
        app.unsaved_form_dialog_opened(PendingNavigation::Select {
            target: LogicalPath::root(disk_entry_name("discovery")),
            kind: EntryKind::Requirement,
        });

        app.unsaved_form_dialog_cancelled();

        assert!(app.unsaved_form_dialog.is_none());
        // Still on "definition" — nothing navigated.
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("definition"))));
    }

    #[test]
    fn unsaved_form_dialog_confirmed_with_nothing_pending_does_nothing() {
        let mut app = test_app();
        let selection_before = app.selection.clone();
        app.unsaved_form_dialog_confirmed();
        assert_eq!(app.selection, selection_before);
    }

    #[test]
    fn unsaved_form_dialog_confirmed_runs_the_pending_select() {
        let mut app = app_editing_a_requirement();
        app.unsaved_form_dialog_opened(PendingNavigation::Select {
            target: LogicalPath::root(disk_entry_name("discovery")),
            kind: EntryKind::Requirement,
        });

        app.unsaved_form_dialog_confirmed();

        assert!(app.unsaved_form_dialog.is_none());
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("discovery"))));
    }

    #[test]
    fn unsaved_form_dialog_confirmed_runs_a_pending_new_requirement() {
        let mut app = app_editing_a_requirement();
        app.unsaved_form_dialog_opened(PendingNavigation::NewRequirement);

        app.unsaved_form_dialog_confirmed();

        assert!(app.unsaved_form_dialog.is_none());
        let EditorState::NewRequirement(form) = &app.editor else {
            panic!("expected a blank NewRequirement form");
        };
        // A fresh create-mode form, not still editing "definition".
        assert!(form.editing_target.is_none());
    }

    #[test]
    fn a_stale_entry_detail_reply_is_ignored_after_a_new_selection() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("first")), EntryKind::Requirement);
        let first_request = app.detail_request.unwrap();
        app.select(LogicalPath::root(disk_entry_name("second")), EntryKind::Requirement);

        // The first selection's reply arrives late, after the user already
        // moved on to a second selection.
        app.apply_event(Event::Completed {
            request: first_request,
            outcome: Outcome::EntryDetail(Some(gui_core::EntryDetail::Requirement {
                title: "First".to_string(),
                requirement_text: String::new(),
                requirement_guidance: None,
                test_guidance: None,
                dependencies: Vec::new(),
                attachments: Vec::new(),
                met_status: gui_core::RequirementMetStatus::Unvalidated,
                original: Box::new(gui_core::RequirementDraft::new("First")),
            })),
        });

        assert!(matches!(app.editor, EditorState::None));
        assert_eq!(app.selection, Some(LogicalPath::root(disk_entry_name("second"))));
    }

    #[test]
    fn save_clicked_sends_a_save_command_and_tracks_it_as_pending() {
        let mut app = test_app();
        app.save_clicked();
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn undo_clicked_sends_an_undo_command_and_tracks_it_as_pending() {
        let mut app = test_app();
        app.undo_clicked();
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn redo_clicked_sends_a_redo_command_and_tracks_it_as_pending() {
        let mut app = test_app();
        app.redo_clicked();
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn a_successful_undo_marks_the_project_dirty() {
        let mut app = test_app();
        app.undo_clicked();
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Undo(Ok(())),
        });

        assert!(app.dirty);
    }

    #[test]
    fn a_failed_undo_does_not_mark_the_project_dirty() {
        let mut app = test_app();
        app.undo_clicked();
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Undo(Err(gui_core::UndoError::NothingToUndo)),
        });

        assert!(!app.dirty);
    }

    #[test]
    fn a_successful_redo_marks_the_project_dirty() {
        let mut app = test_app();
        app.redo_clicked();
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Redo(Ok(())),
        });

        assert!(app.dirty);
    }

    #[test]
    fn validate_clicked_sends_a_validate_command_and_tracks_it_as_pending() {
        let mut app = test_app();
        app.validate_clicked();
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn open_project_sends_a_load_project_command_and_tracks_it_as_pending() {
        let mut app = test_app();
        app.open_project(std::path::PathBuf::from("/some/project"));
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn save_project_as_sends_a_save_as_command_and_tracks_it_as_pending() {
        let mut app = test_app();
        app.save_project_as(std::path::PathBuf::from("/some/project"));
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn a_not_validated_save_opens_the_validate_before_save_dialog() {
        let mut app = test_app();
        app.save_clicked();
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Save(Err(gui_core::SaveError::NotValidated)),
        });

        assert_eq!(
            app.validate_before_save_dialog,
            Some(ValidateBeforeSaveDialogState::Asking { action: PendingSaveAction::Save })
        );
    }

    #[test]
    fn a_not_validated_save_as_opens_the_dialog_carrying_the_target_path() {
        let mut app = test_app();
        let path = std::path::PathBuf::from("/some/project");
        app.save_project_as(path.clone());
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::SaveAs(Err(gui_core::SaveError::NotValidated)),
        });

        assert_eq!(
            app.validate_before_save_dialog,
            Some(ValidateBeforeSaveDialogState::Asking { action: PendingSaveAction::SaveAs(path) })
        );
        // A failed SaveAs must not adopt the path — same guarantee
        // `a_failed_save_as_does_not_change_the_known_project_path`
        // exercises on the gui-core side.
        assert_eq!(app.project_path, None);
    }

    #[test]
    fn confirming_the_validate_before_save_dialog_sends_validate_and_tracks_it() {
        let mut app = test_app();
        app.save_clicked();
        let save_request = app.next_request;
        app.apply_event(Event::Completed {
            request: save_request,
            outcome: Outcome::Save(Err(gui_core::SaveError::NotValidated)),
        });

        app.validate_before_save_confirmed();

        let validate_request = app.next_request;
        assert_eq!(
            app.validate_before_save_dialog,
            Some(ValidateBeforeSaveDialogState::Validating {
                request: validate_request,
                action: PendingSaveAction::Save
            })
        );
        assert!(app.pending.contains_key(&validate_request));
    }

    #[test]
    fn cancelling_the_validate_before_save_dialog_just_closes_it() {
        let mut app = test_app();
        app.save_clicked();
        let request = app.next_request;
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Save(Err(gui_core::SaveError::NotValidated)),
        });

        app.validate_before_save_dismissed();

        assert_eq!(app.validate_before_save_dialog, None);
    }

    #[test]
    fn a_successful_validate_from_the_dialog_closes_it_and_retries_save_as() {
        let mut app = test_app();
        let path = std::path::PathBuf::from("/some/project");
        app.save_project_as(path.clone());
        let save_as_request = app.next_request;
        app.apply_event(Event::Completed {
            request: save_as_request,
            outcome: Outcome::SaveAs(Err(gui_core::SaveError::NotValidated)),
        });
        app.validate_before_save_confirmed();
        let validate_request = app.next_request;

        app.apply_event(Event::Completed {
            request: validate_request,
            outcome: Outcome::Validate(Ok(())),
        });

        assert_eq!(app.validate_before_save_dialog, None);
        // The completed Validate request is cleared, and the retried
        // SaveAs is tracked as a new pending request in its place.
        assert_eq!(app.pending.len(), 1);
        assert!(!app.pending.contains_key(&validate_request));
    }

    #[test]
    fn a_failed_validate_from_the_dialog_shows_the_errors_with_no_auto_retry() {
        let mut app = test_app();
        app.save_clicked();
        let save_request = app.next_request;
        app.apply_event(Event::Completed {
            request: save_request,
            outcome: Outcome::Save(Err(gui_core::SaveError::NotValidated)),
        });
        app.validate_before_save_confirmed();
        let validate_request = app.next_request;

        app.apply_event(Event::Completed {
            request: validate_request,
            outcome: Outcome::Validate(Err(vec![logical::validate::ValidationError::DependencyCycle {
                cycle: vec![LogicalPath::root(disk_entry_name("a"))],
            }])),
        });

        match &app.validate_before_save_dialog {
            Some(ValidateBeforeSaveDialogState::Failed { errors }) => {
                assert_eq!(errors.len(), 1);
                assert!(errors[0].contains("dependency cycle"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        app.validate_before_save_dismissed();
        assert_eq!(app.validate_before_save_dialog, None);
    }

    #[test]
    fn a_plain_toolbar_validate_does_not_touch_the_save_dialog() {
        let mut app = test_app();
        app.validate_clicked();
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Validate(Ok(())),
        });

        assert_eq!(app.validate_before_save_dialog, None);
    }

    #[test]
    fn new_project_dialog_lifecycle() {
        let mut app = test_app();
        assert!(app.new_project_dialog.is_none());

        app.new_project_dialog_opened();
        assert_eq!(app.new_project_dialog, Some(String::new()));

        app.new_project_dialog_cancelled();
        assert!(app.new_project_dialog.is_none());

        app.new_project_dialog_opened();
        app.new_project_dialog.as_mut().unwrap().push_str("Scratch");
        app.new_project_dialog_confirmed();
        assert!(app.new_project_dialog.is_none());
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn new_project_dialog_confirmed_without_the_dialog_open_does_nothing() {
        let mut app = test_app();
        app.new_project_dialog_confirmed();
        assert!(app.pending.is_empty());
    }

    #[test]
    fn a_successful_new_project_starts_dirty() {
        let mut app = test_app();
        app.new_project_dialog_opened();
        app.new_project_dialog_confirmed();
        let request = app.next_request;

        // Starts false so this proves `Outcome::NewProject` itself sets
        // it, not some leftover state from an earlier mutation.
        app.dirty = false;
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::NewProject,
        });

        // No path to lose it to yet — see `Outcome::NewProject`'s own
        // doc comment on why this has to start dirty, unlike a freshly
        // loaded project.
        assert!(app.dirty);
    }

    #[test]
    fn a_successful_new_project_opens_the_root_view_page() {
        let mut app = test_app();
        app.new_project_dialog_opened();
        app.new_project_dialog_confirmed();
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::NewProject,
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(form.path.is_empty());
    }

    #[test]
    fn a_successful_save_as_clears_dirty() {
        let mut app = test_app();
        app.save_project_as(std::path::PathBuf::from("/some/project"));
        let request = app.next_request;

        app.dirty = true;
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::SaveAs(Ok(())),
        });

        assert!(!app.dirty);
    }

    #[test]
    fn needs_path_before_saving_starts_true() {
        let app = test_app();
        assert!(app.needs_path_before_saving());
    }

    #[test]
    fn a_successful_save_as_sets_the_known_project_path() {
        let mut app = test_app();
        app.save_project_as(std::path::PathBuf::from("/some/project"));
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::SaveAs(Ok(())),
        });

        assert!(!app.needs_path_before_saving());
    }

    #[test]
    fn a_failed_save_as_does_not_set_the_known_project_path() {
        let mut app = test_app();
        app.save_project_as(std::path::PathBuf::from("/some/project"));
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::SaveAs(Err(gui_core::SaveError::NotValidated)),
        });

        assert!(app.needs_path_before_saving());
    }

    #[test]
    fn a_successful_load_project_sets_the_known_project_path() {
        let mut app = test_app();
        app.open_project(std::path::PathBuf::from("/some/project"));
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::LoadProject(Ok(())),
        });

        assert!(!app.needs_path_before_saving());
    }

    #[test]
    fn a_successful_load_project_opens_the_root_view_page() {
        let mut app = test_app();
        app.open_project(std::path::PathBuf::from("/some/project"));
        let request = app.next_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::LoadProject(Ok(())),
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(form.path.is_empty());
    }

    #[test]
    fn a_tree_changed_event_refreshes_an_open_root_pages_display_name() {
        // No `self.tree` yet (`select_module` falls back to the path's own
        // last segment when it's `None` — for the root, an empty string,
        // since `path: []` has no last segment) — this is exactly the gap
        // between a `LoadProject` `Outcome` and its own `TreeChanged`
        // (`gui-core::Actor::apply_completion`'s ordering) this arm exists
        // to close.
        let mut app = test_app();
        app.select_module(Vec::new());
        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert_eq!(form.display_name, "");

        app.apply_event(Event::TreeChanged(TreeSnapshot {
            root: TreeNode {
                name: disk_entry_name("Capstone"),
                kind: EntryKind::Module,
                status: gui_core::EntryStatus::Unvalidated,
                children: Vec::new(),
            },
            can_undo: false,
            can_redo: false,
        }));

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert_eq!(form.display_name, "Capstone");
    }

    #[test]
    fn a_stale_load_project_reply_does_not_overwrite_a_newer_pending_path() {
        let mut app = test_app();
        app.open_project(std::path::PathBuf::from("/first"));
        let first_request = app.next_request;
        app.open_project(std::path::PathBuf::from("/second"));

        // The first load's reply arrives late, after the user already
        // moved on to opening a second project — must not resolve
        // `project_path` at all (right or wrong), since `pending_project_path`
        // now belongs to the second request.
        app.apply_event(Event::Completed {
            request: first_request,
            outcome: Outcome::LoadProject(Ok(())),
        });

        assert!(app.needs_path_before_saving());
    }

    #[test]
    fn new_project_clears_an_already_known_project_path() {
        let mut app = test_app();
        app.save_project_as(std::path::PathBuf::from("/some/project"));
        let request = app.next_request;
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::SaveAs(Ok(())),
        });
        assert!(!app.needs_path_before_saving());

        app.new_project_dialog_opened();
        app.new_project_dialog_confirmed();

        assert!(app.needs_path_before_saving());
    }

    #[test]
    fn a_completion_clears_its_own_pending_entry() {
        let mut app = test_app();
        app.save_clicked();
        assert_eq!(app.pending.len(), 1);
        let request = app.next_request; // save_clicked used the counter's current value

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::Save(Ok(())),
        });

        assert!(app.pending.is_empty());
    }

    #[test]
    fn new_module_clicked_opens_a_blank_form() {
        let mut app = test_app();
        app.new_module_clicked();
        assert!(matches!(app.editor, EditorState::NewModule(_)));
    }

    #[test]
    fn editor_cancel_clicked_closes_whatever_form_is_open() {
        let mut app = test_app();
        app.new_requirement_clicked();
        app.editor_cancel_clicked();
        assert!(matches!(app.editor, EditorState::None));
    }

    #[test]
    fn editor_create_clicked_sends_a_command_and_marks_the_form_pending() {
        let mut app = test_app();
        app.new_module_clicked();
        let EditorState::NewModule(form) = &mut app.editor else {
            unreachable!()
        };
        form.name = "scratch".to_string();

        app.editor_create_clicked();

        let EditorState::NewModule(form) = &app.editor else {
            panic!("form should still be open while pending");
        };
        assert!(form.pending_request.is_some());
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn a_successful_create_closes_the_form_and_marks_dirty() {
        let mut app = test_app();
        app.new_module_clicked();
        app.editor_create_clicked();
        let EditorState::NewModule(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::AddModule(Ok(())),
        });

        assert!(matches!(app.editor, EditorState::None));
        assert!(app.dirty);
    }

    #[test]
    fn a_failed_create_reports_the_error_and_leaves_the_form_open() {
        let mut app = test_app();
        app.new_module_clicked();
        app.editor_create_clicked();
        let EditorState::NewModule(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::AddModule(Err(gui_core::AddChildError::ModuleNotFound)),
        });

        let EditorState::NewModule(form) = &app.editor else {
            panic!("form should stay open after a failed create");
        };
        assert!(form.error.is_some());
        assert!(form.pending_request.is_none());
    }

    #[test]
    fn a_create_reply_for_an_already_cancelled_form_is_ignored() {
        let mut app = test_app();
        app.new_module_clicked();
        app.editor_create_clicked();
        let EditorState::NewModule(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();
        app.editor_cancel_clicked();

        // The reply for the now-abandoned form arrives late.
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::AddModule(Ok(())),
        });

        // Still closed, not resurrected, and not incorrectly marked dirty
        // from a stale reply.
        assert!(matches!(app.editor, EditorState::None));
    }

    #[test]
    fn new_entry_module_path_defaults_to_root_then_follows_the_selection() {
        let mut app = test_app();
        assert!(app.new_entry_module_path().is_empty());

        app.selected_module = vec![disk_entry_name("setup")];
        assert_eq!(app.new_entry_module_path(), vec![disk_entry_name("setup")]);
    }

    #[test]
    fn select_module_sets_selected_module_and_clears_the_leaf_selection() {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);
        assert!(app.selection.is_some());

        app.select_module(vec![disk_entry_name("setup")]);

        assert_eq!(app.selected_module, vec![disk_entry_name("setup")]);
        assert!(app.selection.is_none());
        assert!(matches!(app.editor, EditorState::ExistingModule(_)));
    }

    #[test]
    fn selecting_a_leaf_keeps_selected_module_in_sync() {
        let mut app = test_app();

        app.select(
            LogicalPath {
                modules: vec![disk_entry_name("setup")],
                name: disk_entry_name("something"),
            },
            EntryKind::Requirement,
        );

        assert_eq!(app.selected_module, vec![disk_entry_name("setup")]);
    }

    /// Completes whichever `GetEntryDetail` request `app` is currently
    /// waiting on with a matching `EntryDetail::Requirement` reply for
    /// "definition" — shared by `app_editing_a_requirement` (which calls
    /// this twice, once for the initial View and once more after clicking
    /// Edit, since both are real `GetEntryDetail` round trips) and any
    /// other test that needs to land a requirement's detail reply.
    fn complete_definition_requirement_detail(app: &mut GuiApp) {
        let request = app.detail_request.unwrap();
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::EntryDetail(Some(gui_core::EntryDetail::Requirement {
                title: "Definition".to_string(),
                requirement_text: String::new(),
                requirement_guidance: None,
                test_guidance: None,
                dependencies: Vec::new(),
                attachments: Vec::new(),
                met_status: gui_core::RequirementMetStatus::Unvalidated,
                original: Box::new(gui_core::RequirementDraft::new("Definition")),
            })),
        });
    }

    /// Selects `definition`, applies a matching `EntryDetail::Requirement`
    /// reply (landing on the read-only viewer, per `select`'s
    /// `NavMode::View`), then clicks "Edit" and applies its own
    /// `GetEntryDetail` reply too — the same two-step round trip a real
    /// user takes to reach the editable form, and the starting point for
    /// every "editing an existing requirement" test below. Leaves
    /// `app.editor` as `NewRequirement` with `editing_target: Some(_)`,
    /// `read_only: false`.
    fn app_editing_a_requirement() -> GuiApp {
        let mut app = test_app();
        app.select(LogicalPath::root(disk_entry_name("definition")), EntryKind::Requirement);
        complete_definition_requirement_detail(&mut app);
        app.editor_edit_clicked();
        complete_definition_requirement_detail(&mut app);
        app
    }

    #[test]
    fn editor_create_clicked_on_an_editing_form_sends_an_update_not_an_add() {
        let mut app = app_editing_a_requirement();

        app.editor_create_clicked();

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.pending_request.is_some());
        // Still in edit mode, still pointed at the same entry — Save
        // doesn't lose track of what's being edited.
        assert_eq!(form.editing_target, Some(LogicalPath::root(disk_entry_name("definition"))));
    }

    #[test]
    fn a_successful_update_keeps_the_form_open_and_marks_dirty() {
        let mut app = app_editing_a_requirement();
        app.editor_create_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::UpdateRequirement(Ok(())),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            panic!("form should stay open after a successful update");
        };
        assert!(form.pending_request.is_none());
        assert!(form.error.is_none());
        assert!(app.dirty);
    }

    #[test]
    fn a_failed_update_reports_the_error_and_keeps_the_form_open() {
        let mut app = app_editing_a_requirement();
        app.editor_create_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::UpdateRequirement(Err(gui_core::UpdateChildError::NotFound)),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            panic!("form should stay open after a failed update");
        };
        assert!(form.error.is_some());
        assert!(form.pending_request.is_none());
        assert!(!app.dirty);
    }

    #[test]
    fn an_update_reply_for_an_already_closed_form_is_ignored() {
        let mut app = app_editing_a_requirement();
        app.editor_create_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();
        app.editor_cancel_clicked();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::UpdateRequirement(Ok(())),
        });

        // Still closed, and not incorrectly marked dirty from a stale
        // reply for a form the user already backed out of.
        assert!(matches!(app.editor, EditorState::None));
        assert!(!app.dirty);
    }

    #[test]
    fn attachments_dialog_opened_sends_a_get_module_pools_request() {
        let mut app = test_app();

        app.attachments_dialog_opened();

        let dialog = app.attachments_dialog.as_ref().unwrap();
        assert!(dialog.loading);
        assert!(dialog.module.is_empty());
        assert!(app.pools_request.is_some());
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn a_matching_module_pools_reply_populates_the_dialog() {
        let mut app = test_app();
        app.attachments_dialog_opened();
        let request = app.pools_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::ModulePools(Some(gui_core::ModulePools {
                attachments: vec![PathBuf::from("glossary.md")],
                templates: vec![],
            })),
        });

        let dialog = app.attachments_dialog.as_ref().unwrap();
        assert!(!dialog.loading);
        assert_eq!(dialog.attachments, vec![PathBuf::from("glossary.md")]);
    }

    #[test]
    fn a_module_pools_reply_of_none_closes_the_dialog() {
        let mut app = test_app();
        app.attachments_dialog_opened();
        let request = app.pools_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::ModulePools(None),
        });

        assert!(app.attachments_dialog.is_none());
    }

    #[test]
    fn attachments_dialog_closed_clears_the_dialog() {
        let mut app = test_app();
        app.attachments_dialog_opened();

        app.attachments_dialog_closed();

        assert!(app.attachments_dialog.is_none());
    }

    #[test]
    fn add_attachment_clicked_sends_the_command_and_clears_the_input() {
        let mut app = test_app();
        app.attachments_dialog_opened();
        app.attachments_dialog.as_mut().unwrap().new_attachment_path = "notes.md".to_string();

        app.attachments_dialog_add_attachment_clicked();

        assert!(app.attachments_dialog.as_ref().unwrap().new_attachment_path.is_empty());
        // One pending from opening (GetModulePools) plus one for the add.
        assert_eq!(app.pending.len(), 2);
    }

    #[test]
    fn add_attachment_clicked_with_an_empty_path_does_nothing() {
        let mut app = test_app();
        app.attachments_dialog_opened();

        app.attachments_dialog_add_attachment_clicked();

        // Only the original GetModulePools request from opening.
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn a_successful_pool_change_marks_dirty_and_refetches() {
        let mut app = test_app();
        app.attachments_dialog_opened();
        let first_request = app.pools_request.unwrap();
        app.apply_event(Event::Completed {
            request: first_request,
            outcome: Outcome::ModulePools(Some(gui_core::ModulePools {
                attachments: Vec::new(),
                templates: Vec::new(),
            })),
        });

        app.apply_outcome(999, Outcome::AddAttachment(Ok(())));

        assert!(app.dirty);
        // A fresh GetModulePools was sent to refresh the list, so
        // pools_request has moved on from the original.
        assert_ne!(app.pools_request, Some(first_request));
        assert!(app.attachments_dialog.is_some());
    }

    #[test]
    fn a_failed_pool_change_reports_the_error_inline() {
        let mut app = test_app();
        app.attachments_dialog_opened();

        app.apply_outcome(999, Outcome::AddAttachment(Err(gui_core::AddPoolChildError::ModuleNotFound)));

        let dialog = app.attachments_dialog.as_ref().unwrap();
        assert!(dialog.error.is_some());
        assert!(!app.dirty);
    }

    #[test]
    fn pool_change_outcomes_are_ignored_when_no_dialog_is_open() {
        let mut app = test_app();
        assert!(app.attachments_dialog.is_none());

        app.apply_outcome(999, Outcome::AddAttachment(Ok(())));

        assert!(!app.dirty);
        assert!(app.attachments_dialog.is_none());
    }

    #[test]
    fn local_attachment_add_clicked_sends_the_command_and_clears_the_input() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.new_attachment_path = "notes.md".to_string();

        app.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.new_attachment_path.is_empty());
        assert_eq!(app.local_pool_ops.len(), 1);
    }

    #[test]
    fn local_attachment_add_clicked_with_an_empty_path_does_nothing() {
        let mut app = app_editing_a_requirement();

        app.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);

        assert!(app.local_pool_ops.is_empty());
    }

    #[test]
    fn a_successful_local_attachment_add_updates_the_form_in_place() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.new_attachment_path = "notes.md".to_string();
        app.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);
        let request = *app.local_pool_ops.keys().next().unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::AddRequirementAttachment(Ok(())),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert_eq!(form.attachments, vec![PathBuf::from("notes.md")]);
        assert!(app.dirty);
        assert!(app.local_pool_ops.is_empty());
    }

    #[test]
    fn a_successful_local_attachment_remove_updates_the_form_in_place() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.attachments = vec![PathBuf::from("notes.md")];

        app.local_attachment_remove_clicked(LocalPoolKind::RequirementAttachment, PathBuf::from("notes.md"));
        let request = *app.local_pool_ops.keys().next().unwrap();
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RemoveRequirementAttachment(true),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.attachments.is_empty());
    }

    #[test]
    fn a_failed_local_attachment_add_reports_the_error_inline() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.new_attachment_path = "notes.md".to_string();
        app.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);
        let request = *app.local_pool_ops.keys().next().unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::AddRequirementAttachment(Err(gui_core::AddLocalPoolError::EntryNotFound)),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.attachments.is_empty());
        assert!(form.local_pool_error.is_some());
        assert!(!app.dirty);
    }

    #[test]
    fn validate_completion_refreshes_the_open_requirements_met_status() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(matches!(form.met_status, gui_core::RequirementMetStatus::Unvalidated));

        app.apply_event(Event::Completed {
            request: 999,
            outcome: Outcome::Validate(Ok(())),
        });
        let request = app.met_status_request.expect("Validate should have requested a met_status refresh");

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RequirementMetStatus(gui_core::RequirementMetStatus::Met),
        });

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(matches!(form.met_status, gui_core::RequirementMetStatus::Met));
        assert!(app.met_status_request.is_none());
    }

    #[test]
    fn validate_completion_does_nothing_for_a_creation_mode_form() {
        let mut app = test_app();
        app.new_requirement_clicked();

        app.apply_event(Event::Completed {
            request: 999,
            outcome: Outcome::Validate(Ok(())),
        });

        // No `editing_target` to refresh a `met_status` for.
        assert!(app.met_status_request.is_none());
    }

    #[test]
    fn validate_completion_refreshes_an_open_module_pages_summary() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        // Clear the stale request `select_module` itself already sent, so
        // the assertion below can only be satisfied by `Validate`'s own
        // refresh.
        app.module_summary_request = None;

        app.apply_event(Event::Completed {
            request: 999,
            outcome: Outcome::Validate(Ok(())),
        });

        let request = app.module_summary_request.expect("Validate should have requested a summary refresh");

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::ModuleSummary(Some(gui_core::ModuleSummary {
                validated: true,
                requirement_count: 2,
                requirements_met: 1,
                ..Default::default()
            })),
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        let summary = form.summary.as_ref().expect("summary should be populated");
        assert!(summary.validated);
        assert_eq!(summary.requirements_met, 1);
    }

    #[test]
    fn validate_completion_does_nothing_for_a_creation_mode_module_form() {
        let mut app = test_app();
        app.new_module_clicked();

        app.apply_event(Event::Completed {
            request: 999,
            outcome: Outcome::Validate(Ok(())),
        });

        // `NewModule` (creation) has no `ExistingModule` page to refresh.
        assert!(app.module_summary_request.is_none());
    }

    #[test]
    fn refresh_stale_test_references_clicked_sends_the_command_and_tracks_it_as_pending() {
        let mut app = app_editing_a_requirement();

        app.refresh_stale_test_references_clicked();

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.pending_request.is_some());
        assert!(form.error.is_none());
    }

    #[test]
    fn refresh_stale_test_references_clicked_does_nothing_for_a_creation_mode_form() {
        let mut app = test_app();
        app.new_requirement_clicked();

        app.refresh_stale_test_references_clicked();

        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        // No `editing_target` — nothing to send a command against.
        assert!(form.pending_request.is_none());
    }

    #[test]
    fn a_successful_refresh_marks_dirty_and_refetches_entry_detail() {
        let mut app = app_editing_a_requirement();
        app.refresh_stale_test_references_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();
        assert!(!app.dirty);

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RefreshStaleTestReferences(Ok(())),
        });

        assert!(app.dirty);
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.pending_request.is_none());
        assert!(form.error.is_none());
        // Re-fetches the full detail — same "always re-fetch rather than
        // trust stale local state" convention `select_from_history` uses.
        assert!(app.detail_request.is_some());
    }

    #[test]
    fn a_failed_refresh_shows_the_error_and_does_not_refetch() {
        let mut app = app_editing_a_requirement();
        app.refresh_stale_test_references_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();
        let stale_detail_request = app.detail_request;

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RefreshStaleTestReferences(Err(gui_core::RefreshStaleTestReferencesError::NotValidated)),
        });

        assert!(!app.dirty);
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.pending_request.is_none());
        assert!(form.error.is_some());
        // No re-fetch on failure — nothing on disk/in the draft actually
        // changed.
        assert_eq!(app.detail_request, stale_detail_request);
    }

    #[test]
    fn a_stale_refresh_reply_is_ignored_after_the_form_is_closed() {
        let mut app = app_editing_a_requirement();
        app.refresh_stale_test_references_clicked();
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = form.pending_request.unwrap();
        app.editor_cancel_clicked();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RefreshStaleTestReferences(Ok(())),
        });

        // Still closed — the reply didn't resurrect a form the user
        // already navigated away from.
        assert!(matches!(app.editor, EditorState::None));
    }

    #[test]
    fn a_stale_met_status_reply_is_ignored_after_the_form_is_closed() {
        let mut app = app_editing_a_requirement();
        app.apply_event(Event::Completed {
            request: 999,
            outcome: Outcome::Validate(Ok(())),
        });
        let request = app.met_status_request.expect("Validate should have requested a met_status refresh");
        app.editor_cancel_clicked();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RequirementMetStatus(gui_core::RequirementMetStatus::Met),
        });

        // Still closed — the reply didn't resurrect a form the user
        // already navigated away from.
        assert!(matches!(app.editor, EditorState::None));
    }

    #[test]
    fn a_stale_local_attachment_reply_is_ignored_after_the_form_is_closed() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.new_attachment_path = "notes.md".to_string();
        app.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);
        let request = *app.local_pool_ops.keys().next().unwrap();
        app.editor_cancel_clicked();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::AddRequirementAttachment(Ok(())),
        });

        // Still closed — the reply didn't resurrect or repopulate a form
        // the user already navigated away from.
        assert!(matches!(app.editor, EditorState::None));
    }

    #[test]
    fn a_stale_commit_fetch_reply_is_ignored_after_the_form_is_closed() {
        let mut app = app_editing_a_requirement();
        app.dependency_commit_auto_clicked(
            DependencySlot::New,
            AutoCommitKind::Local(LogicalPath::root(disk_entry_name("discovery"))),
        );
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        let request = *form.pending_commit_fetches.keys().next().unwrap();
        app.editor_cancel_clicked();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::ResolveLocalCommit(Ok("deadbeef".to_string())),
        });

        // Still closed — same "the reply didn't resurrect a form the user
        // already navigated away from" precedent as the local-attachment
        // reply above.
        assert!(matches!(app.editor, EditorState::None));
    }

    #[test]
    fn a_stale_commit_fetch_reply_for_a_removed_dependency_is_ignored() {
        let mut app = app_editing_a_requirement();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        form.dependencies.push(DependencyDraft::LocalRequirement {
            path: "/requirements/discovery".to_string(),
            commit: String::new(),
        });
        app.dependency_commit_auto_clicked(
            DependencySlot::Existing(0),
            AutoCommitKind::Local(LogicalPath::root(disk_entry_name("discovery"))),
        );
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        let request = *form.pending_commit_fetches.keys().next().unwrap();
        // The row itself is gone by the time the reply lands — a
        // different stale-reply shape than the form closing outright
        // (`a_stale_commit_fetch_reply_is_ignored_after_the_form_is_closed`
        // above): the form's still open, just with nothing left at that
        // index for `apply_commit_fetch_result` to write into.
        form.dependencies.remove(0);

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::ResolveLocalCommit(Ok("deadbeef".to_string())),
        });

        // No panic, and nothing resurrected the removed row.
        let EditorState::NewRequirement(form) = &app.editor else {
            unreachable!()
        };
        assert!(form.dependencies.is_empty());
    }

    #[test]
    fn local_attachment_add_clicked_does_nothing_for_a_creation_mode_form() {
        let mut app = test_app();
        app.new_requirement_clicked();
        let EditorState::NewRequirement(form) = &mut app.editor else {
            unreachable!()
        };
        assert!(form.editing_target.is_none());
        form.new_attachment_path = "notes.md".to_string();

        app.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);

        // No editing_target to send an attachment command against.
        assert!(app.local_pool_ops.is_empty());
    }

    #[test]
    fn select_module_opens_the_view_page_and_fetches_its_summary() {
        let mut app = test_app();

        app.select_module(vec![disk_entry_name("setup")]);

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert_eq!(form.path, vec![disk_entry_name("setup")]);
        assert!(form.read_only);
        assert!(form.summary.is_none());
        assert!(app.module_summary_request.is_some());
        // 2: the `GetModuleSummary` request plus the sidebar's own
        // `GetModulePools` fetch (`fetch_sidebar_pools`), both sent by
        // `select_module`.
        assert_eq!(app.pending.len(), 2);
    }

    #[test]
    fn selecting_the_root_opens_the_view_page_with_an_empty_path() {
        let mut app = test_app();

        app.select_module(Vec::new());

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(form.path.is_empty());
    }

    #[test]
    fn a_module_summary_reply_populates_the_page() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        let request = app.module_summary_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::ModuleSummary(Some(gui_core::ModuleSummary {
                requirement_count: 3,
                ..Default::default()
            })),
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert_eq!(form.summary.as_ref().unwrap().requirement_count, 3);
        assert!(app.module_summary_request.is_none());
    }

    #[test]
    fn a_stale_module_summary_reply_is_ignored_after_navigating_away() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        let stale_request = app.module_summary_request.unwrap();

        app.select_module(vec![disk_entry_name("other")]);

        app.apply_event(Event::Completed {
            request: stale_request,
            outcome: Outcome::ModuleSummary(Some(gui_core::ModuleSummary::default())),
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        // Still the second selection, untouched by the first's stale reply.
        assert_eq!(form.path, vec![disk_entry_name("other")]);
        assert!(form.summary.is_none());
    }

    #[test]
    fn editor_edit_clicked_switches_the_module_page_to_editable() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);

        app.editor_edit_clicked();

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(!form.read_only);
        assert_eq!(form.new_name, form.display_name);
    }

    #[test]
    fn editor_cancel_clicked_reverts_the_module_page_to_read_only() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        app.editor_edit_clicked();
        if let EditorState::ExistingModule(form) = &mut app.editor {
            form.new_name = "scratch".to_string();
            form.edited = true;
        }

        app.editor_cancel_clicked();

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(form.read_only);
        assert!(!form.edited);
    }

    #[test]
    fn editor_create_clicked_on_a_nested_module_page_sends_the_rename() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        app.editor_edit_clicked();
        if let EditorState::ExistingModule(form) = &mut app.editor {
            form.new_name = "renamed".to_string();
        }

        app.editor_create_clicked();

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(form.pending_request.is_some());
        // 3: the `GetModuleSummary` and `GetModulePools` requests
        // `select_module` itself sent, plus this rename.
        assert_eq!(app.pending.len(), 3);
    }

    #[test]
    fn a_successful_module_rename_updates_the_path_and_returns_to_the_view() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        app.editor_edit_clicked();
        if let EditorState::ExistingModule(form) = &mut app.editor {
            form.new_name = "renamed".to_string();
        }
        app.editor_create_clicked();
        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        let request = form.pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RenameModule(Ok(())),
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert_eq!(form.path, vec![disk_entry_name("renamed")]);
        assert_eq!(form.display_name, "renamed");
        assert!(form.read_only);
        assert_eq!(app.selected_module, vec![disk_entry_name("renamed")]);
        assert!(app.dirty);
    }

    #[test]
    fn a_failed_module_rename_reports_the_error_and_stays_in_edit_mode() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        app.editor_edit_clicked();
        if let EditorState::ExistingModule(form) = &mut app.editor {
            form.new_name = "renamed".to_string();
        }
        app.editor_create_clicked();
        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        let request = form.pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RenameModule(Err(gui_core::RenameModuleError::NotFound)),
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(form.error.is_some());
        assert!(!form.read_only);
        assert!(!app.dirty);
    }

    #[test]
    fn a_successful_project_rename_leaves_the_root_path_empty() {
        let mut app = test_app();
        app.select_module(Vec::new());
        app.editor_edit_clicked();
        if let EditorState::ExistingModule(form) = &mut app.editor {
            form.new_name = "Renamed Project".to_string();
        }
        app.editor_create_clicked();
        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        let request = form.pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RenameProject(Ok(())),
        });

        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule");
        };
        assert!(form.path.is_empty());
        assert_eq!(form.display_name, "Renamed Project");
        assert_eq!(app.selected_module, Vec::<gui_core::EntryName>::new());
    }

    #[test]
    fn editor_delete_clicked_on_an_editing_requirement_opens_the_confirmation() {
        let mut app = app_editing_a_requirement();

        app.editor_delete_clicked();

        let dialog = app.delete_confirm_dialog.as_ref().expect("dialog should be open");
        assert_eq!(dialog.target, DeleteTarget::Requirement(LogicalPath::root(disk_entry_name("definition"))));
        assert_eq!(dialog.label, "definition");
        assert!(dialog.pending_request.is_none());
    }

    #[test]
    fn editor_delete_clicked_is_a_no_op_for_a_create_mode_form() {
        let mut app = test_app();
        app.new_requirement_clicked();

        app.editor_delete_clicked();

        assert!(app.delete_confirm_dialog.is_none());
    }

    #[test]
    fn editor_delete_clicked_is_a_no_op_for_the_project_root() {
        let mut app = test_app();
        app.select_module(Vec::new());
        app.editor_edit_clicked();

        app.editor_delete_clicked();

        assert!(app.delete_confirm_dialog.is_none());
    }

    #[test]
    fn editor_delete_clicked_on_an_editing_module_opens_the_confirmation() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        app.editor_edit_clicked();

        app.editor_delete_clicked();

        let dialog = app.delete_confirm_dialog.as_ref().expect("dialog should be open");
        assert_eq!(dialog.target, DeleteTarget::Module(vec![disk_entry_name("setup")]));
    }

    #[test]
    fn delete_cancelled_closes_the_dialog_without_sending_anything() {
        let mut app = app_editing_a_requirement();
        app.editor_delete_clicked();
        let pending_before = app.pending.len();

        app.delete_cancelled();

        assert!(app.delete_confirm_dialog.is_none());
        assert_eq!(app.pending.len(), pending_before);
    }

    #[test]
    fn delete_confirmed_sends_the_matching_remove_command_and_tracks_it() {
        let mut app = app_editing_a_requirement();
        app.editor_delete_clicked();

        app.delete_confirmed();

        let dialog = app.delete_confirm_dialog.as_ref().expect("dialog should still be open while pending");
        assert!(dialog.pending_request.is_some());
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn a_successful_delete_closes_the_dialog_marks_dirty_and_selects_the_parent_module() {
        let mut app = app_editing_a_requirement();
        app.editor_delete_clicked();
        app.delete_confirmed();
        let request = app.delete_confirm_dialog.as_ref().unwrap().pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RemoveRequirement(true),
        });

        assert!(app.delete_confirm_dialog.is_none());
        assert!(app.dirty);
        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule after navigating to the parent module");
        };
        assert!(form.path.is_empty());
    }

    #[test]
    fn a_failed_delete_leaves_the_dialog_open_with_an_error() {
        let mut app = app_editing_a_requirement();
        app.editor_delete_clicked();
        app.delete_confirmed();
        let request = app.delete_confirm_dialog.as_ref().unwrap().pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RemoveRequirement(false),
        });

        let dialog = app.delete_confirm_dialog.as_ref().expect("dialog should stay open on failure");
        assert!(dialog.error.is_some());
        assert!(dialog.pending_request.is_none());
        assert!(!app.dirty);
    }

    #[test]
    fn a_delete_reply_for_an_already_cancelled_dialog_is_ignored() {
        let mut app = app_editing_a_requirement();
        app.editor_delete_clicked();
        app.delete_confirmed();
        let request = app.delete_confirm_dialog.as_ref().unwrap().pending_request.unwrap();
        app.delete_cancelled();

        // The reply for the now-abandoned dialog arrives late.
        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RemoveRequirement(true),
        });

        assert!(app.delete_confirm_dialog.is_none());
        assert!(!app.dirty);
        // Still the requirement form — a stale reply must not redirect it.
        assert!(matches!(app.editor, EditorState::NewRequirement(_)));
    }

    #[test]
    fn a_successful_module_delete_selects_the_parent_module() {
        let mut app = test_app();
        app.select_module(vec![disk_entry_name("setup")]);
        app.editor_edit_clicked();
        app.editor_delete_clicked();
        app.delete_confirmed();
        let request = app.delete_confirm_dialog.as_ref().unwrap().pending_request.unwrap();

        app.apply_event(Event::Completed {
            request,
            outcome: Outcome::RemoveModule(true),
        });

        assert!(app.delete_confirm_dialog.is_none());
        let EditorState::ExistingModule(form) = &app.editor else {
            panic!("expected ExistingModule after navigating to the parent module");
        };
        assert!(form.path.is_empty());
    }
}
