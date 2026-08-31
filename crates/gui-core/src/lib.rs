//! See `README.md` for the design this implements: the actor +
//! spawn-per-request pattern, the exactly-one-mutation-at-a-time policy,
//! and the `Command`/`Event` message-passing contract with `gui-ui`.
//!
//! Module-level attachment/template add/remove are wired up (`AddAttachment`/
//! `RemoveAttachment`/`AddTemplate`/`RemoveTemplate`), and so are a
//! requirement/test/result's own *local* attachment pools (`AddRequirement
//! Attachment`, a test's separate local template pool via
//! `AddTestTemplateFile`, etc.) — `gui-ui` doesn't have a form field for
//! the latter yet, so they're implemented here but unused so far.

mod actor;
mod tree;

pub use actor::CoreHandle;
// Re-exported so `gui-ui` can name these in its own `Command`-adjacent code
// (e.g. `self.selection: Option<LogicalPath>`) without taking a direct
// Cargo dependency on `disk`/`logical` itself — see gui-ui/README.md's
// "Dependencies".
pub use disk::{DependencyReferenceKind, EntryName, LocalGitReference, ReferencePath, RemoteGitReference, ResultKindV1};
pub use logical::LogicalPath;
pub use logical::AddPoolFileError;
pub use logical::draft::{RequirementDraft, ResultDraft, TestDraft};

use std::path::PathBuf;

use logical::draft::{AddNamedChildError, ProjectDraft};
use logical::validate::ValidationError;

pub type RequestId = u64;

/// The actor's project state, always exactly one of these two shapes.
/// See README's "Project state: alternating Draft/Validated, one owner".
/// `Clone`, never `Arc`/`Mutex` — every read dispatches against its own
/// `state.clone()`, per README's "`Arc` and `Mutex` are both off the
/// table, by rule".
#[derive(Debug, Clone)]
pub enum ProjectState {
    Draft(ProjectDraft),
    Validated(logical::ValidatedProject),
}

/// See README's "Message types". Split in comments below into mutating
/// commands (subject to the one-at-a-time exclusion policy) and read-only
/// commands (never blocked by each other, only by a mutation in flight).
#[derive(Debug)]
pub enum Command {
    // -- Mutating --
    LoadProject {
        path: PathBuf,
        request: RequestId,
    },
    Validate {
        request: RequestId,
    },
    Save {
        request: RequestId,
    },
    /// Save to a specific path — either re-targeting an already-loaded
    /// project (an actual "Save As") or the *first* save of a project
    /// that has no on-disk home yet (see `NewProject`'s own doc comment:
    /// creating one in memory doesn't pick a path). Same `Validated`
    /// precondition as `Save` (`Outcome::SaveAs`'s `SaveError` is the
    /// same type), just against a caller-supplied directory instead of
    /// whatever `LoadProject`/the last `SaveAs` set.
    SaveAs {
        path: PathBuf,
        request: RequestId,
    },
    /// Starts a brand new project in memory — `logical::draft::
    /// create_project`, an empty `ModuleDraft` under the given name — with
    /// no on-disk path at all. Purely in-memory (no filesystem I/O), so
    /// unlike `LoadProject` this isn't spawned/async: it completes within
    /// one `dispatch` call, same as `AddRequirement` and friends. A
    /// `SaveAs` (not a plain `Save`, which needs a path already) is the
    /// only way to actually persist it.
    NewProject {
        name: String,
        request: RequestId,
    },
    /// Restores the most recent snapshot pushed onto the actor's own
    /// undo stack, pushing the *current* state onto the redo stack first
    /// — see `Actor::push_undo_snapshot`/`undo`. Purely in-memory, same
    /// "inline, not spawned" shape as `NewProject`.
    Undo {
        request: RequestId,
    },
    /// The mirror image of `Undo` — see that variant's own doc comment.
    Redo {
        request: RequestId,
    },
    AddRequirement {
        /// The target module's own path — the chain of submodule names
        /// from the project root down to *and including* the module to
        /// add into (empty means the project root). Deliberately a plain
        /// `Vec<EntryName>`, not a `LogicalPath`: `LogicalPath` names a
        /// requirement/test/result *inside* a module, and has no way to
        /// name the module itself as the target — see README's message
        /// types (this is a correction made while implementing against
        /// that sketch, not a design carried over unchanged).
        module: Vec<EntryName>,
        name: EntryName,
        requirement: Box<RequirementDraft>,
        request: RequestId,
    },
    /// Replaces an *existing* requirement's content wholesale — fails if
    /// nothing is there yet (use `AddRequirement` to create). There's no
    /// `logical`-level "update" operation to call into; this is gui-core's
    /// own direct `BTreeMap` replacement, see `Actor::update_requirement`.
    UpdateRequirement {
        target: LogicalPath,
        requirement: Box<RequirementDraft>,
        request: RequestId,
    },
    RemoveRequirement {
        target: LogicalPath,
        request: RequestId,
    },
    AddTest {
        module: Vec<EntryName>,
        name: EntryName,
        test: Box<TestDraft>,
        request: RequestId,
    },
    UpdateTest {
        target: LogicalPath,
        test: Box<TestDraft>,
        request: RequestId,
    },
    RemoveTest {
        target: LogicalPath,
        request: RequestId,
    },
    AddResult {
        module: Vec<EntryName>,
        name: EntryName,
        result: Box<ResultDraft>,
        request: RequestId,
    },
    UpdateResult {
        target: LogicalPath,
        result: Box<ResultDraft>,
        request: RequestId,
    },
    RemoveResult {
        target: LogicalPath,
        request: RequestId,
    },
    AddModule {
        /// The *parent* module's path — the new module is created as a
        /// child named `name` underneath it, mirroring `AddRequirement`'s
        /// `module` field (which names the module to add *into*, not the
        /// thing being added).
        module: Vec<EntryName>,
        name: EntryName,
        request: RequestId,
    },
    RemoveModule {
        /// The module being removed itself — unlike `AddModule`, there's
        /// no separate "name" to append; `target` already ends at the
        /// module to remove.
        target: Vec<EntryName>,
        request: RequestId,
    },
    /// Renames a module *in place* — `new_name` replaces the last segment
    /// of `target`, keeping the same parent and all of the module's own
    /// content (children, attachments, everything). Not a move to a
    /// different parent; that's a different, bigger operation this
    /// doesn't attempt.
    RenameModule {
        target: Vec<EntryName>,
        new_name: EntryName,
        request: RequestId,
    },
    /// A module-level attachment pool entry — addressed by `module` +
    /// `path` directly, not `EntryName`, since a pool is a `BTreeSet<
    /// PathBuf>`, not a named-child map. No `Update*` counterpart: an
    /// attachment is just a file reference, there's nothing about it to
    /// edit in place beyond adding or removing it.
    AddAttachment {
        module: Vec<EntryName>,
        path: PathBuf,
        request: RequestId,
    },
    RemoveAttachment {
        module: Vec<EntryName>,
        path: PathBuf,
        request: RequestId,
    },
    AddTemplate {
        module: Vec<EntryName>,
        path: PathBuf,
        request: RequestId,
    },
    RemoveTemplate {
        module: Vec<EntryName>,
        path: PathBuf,
        request: RequestId,
    },
    /// A requirement's own local attachment pool — addressed by the
    /// requirement's `target: LogicalPath`, not module + path, since this
    /// is scoped to one entry rather than shared module-wide. No
    /// `RequirementDraft`-side template pool — only tests have one.
    AddRequirementAttachment {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },
    RemoveRequirementAttachment {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },
    AddTestAttachment {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },
    RemoveTestAttachment {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },
    /// A test's local *template* pool — distinct from its attachment
    /// pool; `logical::draft::TestDraft` keeps the two separate
    /// (`add_attachment` vs. `add_template_file`).
    AddTestTemplateFile {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },
    RemoveTestTemplateFile {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },
    AddResultAttachment {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },
    RemoveResultAttachment {
        target: LogicalPath,
        path: PathBuf,
        request: RequestId,
    },

    // -- Read-only --
    GetEntryDetail {
        target: LogicalPath,
        /// Which pool to resolve `target.name` against — a requirement,
        /// test, and result can share the same name within the same
        /// module (a natural pairing, e.g. a result named after the
        /// requirement it reports on), so the lookup can't just try each
        /// pool in turn and return the first hit.
        kind: EntryKind,
        request: RequestId,
    },
    IsRequirementMet {
        target: LogicalPath,
        request: RequestId,
    },
    DependencyChain {
        target: LogicalPath,
        request: RequestId,
    },
    /// The paths currently in a module's attachment/template pools — the
    /// on-demand read `gui-ui`'s attachment-management UI fetches before
    /// showing what's there to add/remove against.
    GetModulePools {
        module: Vec<EntryName>,
        request: RequestId,
    },

    Shutdown,
}

/// Per-command result payload for `Event::Completed`. Each variant carries
/// its own operation's real error type rather than a shared catch-all, one
/// variant per `Command` that can complete with a result.
#[derive(Debug)]
pub enum Outcome {
    LoadProject(Result<(), disk::project::operations::load::Error>),
    Validate(Result<(), Vec<ValidationError>>),
    Save(Result<(), SaveError>),
    /// Same error type as `Save` — see `Command::SaveAs`'s doc comment on
    /// why the two share one underlying operation.
    SaveAs(Result<(), SaveError>),
    /// Can't actually fail — `logical::draft::create_project` just builds
    /// a struct in memory — so unlike every sibling here this carries no
    /// `Result`, just a completion signal.
    NewProject,
    Undo(Result<(), UndoError>),
    Redo(Result<(), RedoError>),
    AddRequirement(Result<(), AddChildError>),
    UpdateRequirement(Result<(), UpdateChildError>),
    /// Whether an entry was actually removed — `remove_*` never fails in
    /// `logical` either, it's just `Option`-shaped; `false` covers both
    /// "no project loaded" and "nothing there by that path".
    RemoveRequirement(bool),
    AddTest(Result<(), AddChildError>),
    UpdateTest(Result<(), UpdateChildError>),
    RemoveTest(bool),
    AddResult(Result<(), AddChildError>),
    UpdateResult(Result<(), UpdateChildError>),
    RemoveResult(bool),
    AddModule(Result<(), AddChildError>),
    RemoveModule(bool),
    RenameModule(Result<(), RenameModuleError>),
    AddAttachment(Result<(), AddPoolChildError>),
    RemoveAttachment(bool),
    AddTemplate(Result<(), AddPoolChildError>),
    RemoveTemplate(bool),
    AddRequirementAttachment(Result<(), AddLocalPoolError>),
    RemoveRequirementAttachment(bool),
    AddTestAttachment(Result<(), AddLocalPoolError>),
    RemoveTestAttachment(bool),
    AddTestTemplateFile(Result<(), AddLocalPoolError>),
    RemoveTestTemplateFile(bool),
    AddResultAttachment(Result<(), AddLocalPoolError>),
    RemoveResultAttachment(bool),
    EntryDetail(Option<EntryDetail>),
    RequirementMet(bool),
    DependencyChain(Vec<LogicalPath>),
    /// `None` means the module itself wasn't found.
    ModulePools(Option<ModulePools>),
    /// A command that needs a loaded project arrived when `state` is
    /// `None` (no `LoadProject` has ever succeeded, or the current
    /// project failed to load).
    NoProjectLoaded,
}

/// `add_requirement`/`add_test`/`add_result`/`add_module` all need
/// somewhere to report "the target module doesn't exist" — a failure mode
/// `logical::draft::AddNamedChildError` has no variant for, since
/// `ModuleDraft::add_*` is only ever called on a module you already have a
/// `&mut` to. This is gui-core's own operation (resolve the module, then
/// add), so it gets its own error type rather than stretching
/// `AddNamedChildError` to cover a case it wasn't designed for. Shared
/// across all four `add_*` commands deliberately, not per-command: they
/// fail for the identical two structural reasons (missing module, name
/// collision), the same way `AddNamedChildError` itself is already one
/// type shared across `ModuleDraft`'s own `add_*` methods (see that type's
/// doc comment in `logical`) — not a case of stretching one function's
/// error type to cover another's unrelated failure modes.
#[derive(Debug, thiserror::Error)]
pub enum AddChildError {
    #[error("module not found")]
    ModuleNotFound,
    #[error(transparent)]
    Add(#[from] AddNamedChildError),
}

/// `add_attachment`/`add_template` need the same "module doesn't exist"
/// case `AddChildError` covers for named children — same reasoning, but a
/// distinct type since the underlying per-command error
/// (`logical::AddPoolFileError`) is a different type from
/// `AddNamedChildError` (pools are `BTreeSet<PathBuf>`, not
/// `BTreeMap<EntryName, _>`). Shared across both `add_attachment`/
/// `add_template` for the same reason `AddChildError` is shared across the
/// named-child `add_*` commands.
#[derive(Debug, thiserror::Error)]
pub enum AddPoolChildError {
    #[error("module not found")]
    ModuleNotFound,
    #[error(transparent)]
    Add(#[from] AddPoolFileError),
}

/// A requirement/test/result's own *local* attachment (or test template)
/// pool add fails for the union of `AddPoolChildError`'s and
/// `UpdateChildError`'s reasons: the containing module doesn't exist, the
/// entry itself doesn't exist within it (this is a local pool — there's no
/// entry to be local *to* otherwise), or the underlying pool add itself
/// fails (bad path, already there). Shared across all four local-pool add
/// commands (requirement/test attachments, test template files, result
/// attachments) for the same reason every other `*ChildError` here is
/// shared across its sibling commands.
#[derive(Debug, thiserror::Error)]
pub enum AddLocalPoolError {
    #[error("module not found")]
    ModuleNotFound,
    #[error("no entry with that name exists")]
    EntryNotFound,
    #[error(transparent)]
    Add(#[from] AddPoolFileError),
}

/// `update_requirement`/`update_test`/`update_result` fail for the mirror
/// image of `AddChildError`'s reasons: the module doesn't exist, or it
/// does but there's nothing there yet by that name (use the matching
/// `add_*` command to create it first). Shared across all three `update_*`
/// commands for the same reason `AddChildError` is shared across `add_*`.
#[derive(Debug, thiserror::Error)]
pub enum UpdateChildError {
    #[error("module not found")]
    ModuleNotFound,
    #[error("no entry with that name exists yet — use add instead")]
    NotFound,
}

/// `rename_module`'s own error type, not shared with anything else —
/// unlike `AddChildError`/`UpdateChildError`/etc., there's no sibling
/// `rename_*` command family for this to be shared across (a requirement/
/// test/result addressed by `EntryName` doesn't need renaming the same
/// way — `update_requirement` etc. already replace the whole entry, name
/// included in spirit, in one step; only a module's own identity *is* its
/// map key with nothing else to replace-in-place instead).
#[derive(Debug, thiserror::Error)]
pub enum RenameModuleError {
    #[error("cannot rename the project root")]
    CannotRenameRoot,
    #[error("module not found")]
    ModuleNotFound,
    #[error("no module with that name exists")]
    NotFound,
    #[error(transparent)]
    Add(#[from] AddNamedChildError),
}

/// `save()` requires a `ValidatedProject` — an unvalidated `Draft` can't
/// be saved at all, per `logical`'s design (see its README's "Draft vs.
/// validated"). `disk::project::operations::save::Error` has no variant
/// for that, since it's a `logical`/`gui-core`-level precondition, not a
/// `disk`-level one.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("project must be validated before it can be saved")]
    NotValidated,
    #[error(transparent)]
    Save(#[from] disk::project::operations::save::Error),
}

/// `Undo`'s own error type per the per-`Command` convention (see
/// `SaveError`'s own doc comment for why this isn't folded into a shared
/// catch-all with `RedoError`, despite the two being structurally
/// identical) — genuinely different operations, even though this crate's
/// undo/redo stacks are each other's mirror image.
#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error("nothing to undo")]
    NothingToUndo,
}

/// `Redo`'s own error type — see `UndoError`'s doc comment.
#[derive(Debug, thiserror::Error)]
pub enum RedoError {
    #[error("nothing to redo")]
    NothingToRedo,
}

/// Full detail for one entry, fetched on demand for the center pane. See
/// README's "`TreeSnapshot` is a simplified read model, not `logical`
/// data" — this is the on-demand counterpart to that snapshot. One variant
/// per leaf kind (no `Module` variant — a module carries no editable
/// content of its own beyond its name/children, which the tree already
/// shows), each carrying exactly the fields gui-ui's matching creation
/// form edits, so a `Command::Update*` can be built directly from an
/// `EntryDetail` gui-ui already has in hand.
#[derive(Debug, Clone)]
pub enum EntryDetail {
    Requirement {
        title: String,
        requirement_text: String,
        requirement_guidance: Option<String>,
        test_guidance: Option<String>,
        /// This requirement's `dependency`/`dependencies` field, collapsed
        /// — same singular/plural-collapse convention `logical`'s own
        /// `RequirementDraft.dependencies` already follows (see `disk`'s
        /// README on the on-disk `dependency`/`dependencies` split).
        /// Ordinary draft data, submitted back whole via
        /// `Command::UpdateRequirement`/`AddRequirement` on Save — unlike
        /// `attachments` below, there's no local-pool round trip for this
        /// (a dependency isn't a file to copy into place, just a
        /// reference).
        dependencies: Vec<DependencyReferenceKind>,
        /// This requirement's own local attachment pool — see
        /// `Command::AddRequirementAttachment`'s doc comment.
        attachments: Vec<PathBuf>,
    },
    Test {
        title: String,
        result_kind: ResultKindV1,
        attachments: Vec<PathBuf>,
        /// Distinct from `attachments` — see `Command::AddTestTemplateFile`'s
        /// doc comment.
        template_files: Vec<PathBuf>,
    },
    Result {
        title: String,
        requirement_path: String,
        requirement_commit: String,
        test_path: String,
        test_commit: String,
        attachments: Vec<PathBuf>,
    },
}

/// A module's two file pools, as currently registered — the on-demand
/// counterpart `GetModulePools` fetches for `gui-ui`'s attachment-
/// management UI, same "purpose-built read, not the raw `logical` data"
/// spirit as `EntryDetail`/`TreeSnapshot`.
#[derive(Debug, Clone)]
pub struct ModulePools {
    pub attachments: Vec<PathBuf>,
    pub templates: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum Event {
    Completed { request: RequestId, outcome: Outcome },
    /// Pushed whenever the actor's tree changes, whether from a command
    /// gui-ui itself issued or (later) an external change.
    TreeChanged(TreeSnapshot),
    ValidationFailed(Vec<ValidationError>),
}

/// Purpose-built read model for the left-pane tree — never the full
/// `ProjectDraft`/`ValidatedProject`. See README's "`TreeSnapshot` is a
/// simplified read model" section. Also carries `can_undo`/`can_redo` —
/// not tree data, but piggybacking on `Event::TreeChanged` (pushed after
/// every mutation, `Undo`/`Redo` included) rather than inventing a
/// separate `Event` variant just for undo/redo availability; see
/// `gui-ui`'s README on why its own toolbar buttons need to know this
/// rather than just always being enabled.
#[derive(Debug, Clone)]
pub struct TreeSnapshot {
    pub root: TreeNode,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: EntryName,
    pub kind: EntryKind,
    pub status: EntryStatus,
    pub children: Vec<TreeNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Module,
    Requirement,
    Test,
    Result,
}

/// Coarse status only — cheap enough to include on every node without a
/// separate read per entry. Full detail is `GetEntryDetail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStatus {
    Unvalidated,
    Met,
    Unmet,
}
