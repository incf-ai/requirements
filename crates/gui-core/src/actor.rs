//! The actor: owns `ProjectState` exclusively, dispatches `Command`s,
//! enforces the one-mutation-at-a-time / concurrent-reads policy. See
//! `README.md`'s "The actor + spawn-per-request pattern" and "Exactly one
//! mutation at a time" sections.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use syscalls::{Filesystem, Git, RemoteGit};
use tokio::sync::mpsc;

use crate::tree::{
    build_tree_snapshot, dependency_chain, find_references, get_entry_detail, get_module_pools, get_module_summary,
    get_requirement_met_status, get_result_reference_is_stale, resolve_module_mut,
};
use crate::{
    AddChildError, AddLocalPoolError, AddPoolChildError, AddPoolFileError, Command, CommitAllError, EntryPath, Event,
    GetChangedFilesError, GetCommitFileDiffError, GetCommitFilesError, GetCommitLogError, GetDiffError,
    GetUnpushedCommitsError, LocalCommitKind, LogicalPath, Outcome, ProjectState, PushError, RedoError,
    ReferencePath, RefreshStaleTestReferencesError, RenameModuleError, RenameProjectError, RequestId,
    ResolveLocalCommitError, SaveError, UndoError, UpdateChildError,
};

/// The boundary `gui-ui` talks across. Plain `Send + Sync`, non-blocking
/// `send`/`try_recv_event` only — see README's "Threading model".
pub struct CoreHandle {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<Event>,
    // Keeps the runtime (and its worker threads) alive for as long as this
    // handle lives; dropping the handle shuts the runtime down.
    _runtime: tokio::runtime::Runtime,
}

impl CoreHandle {
    /// Always talks to the real filesystem/git — `syscalls::StdFilesystem`/
    /// `syscalls::SystemGit`. The actor itself (`run_actor`) is generic
    /// over `Filesystem`/`Git`/`RemoteGit` specifically so tests can
    /// exercise `Validate`/`Save`/`LoadProject` against fakes instead —
    /// see the `test` module below and README's Testing strategy.
    pub fn start() -> Result<CoreHandle, crate::StartError> {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_time().build()?;

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        runtime.spawn(run_actor(
            command_rx,
            event_tx,
            syscalls::StdFilesystem,
            syscalls::SystemGit,
        ));

        Ok(CoreHandle {
            commands: command_tx,
            events: event_rx,
            _runtime: runtime,
        })
    }

    /// Same actor loop as `start()`, but against caller-supplied
    /// `Filesystem`/`Git`/`RemoteGit` implementations instead of the real
    /// `syscalls::StdFilesystem`/`syscalls::SystemGit` — test-support
    /// surface for `gui-ui`'s own tests, which run against a real
    /// `CoreHandle` rather than a fake actor (see that crate's
    /// `tests/interaction.rs`), but sometimes need to control something
    /// `start()` can't, like making a real `Save` artificially slow via
    /// `syscalls::SlowFilesystem` to exercise the exit dialog's
    /// Saving/TimedOut states deterministically.
    pub fn start_with<F, G>(fs: F, git: G) -> Result<CoreHandle, crate::StartError>
    where
        F: Filesystem + Clone + Send + Sync + 'static,
        G: Git + RemoteGit + Clone + Send + Sync + 'static,
    {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_time().build()?;

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        runtime.spawn(run_actor(command_rx, event_tx, fs, git));

        Ok(CoreHandle {
            commands: command_tx,
            events: event_rx,
            _runtime: runtime,
        })
    }

    /// Never blocks — the channel is unbounded specifically so this can't
    /// stall the caller (gui-ui's render thread) waiting for the actor to
    /// drain it. A closed receiver (actor task gone) is swallowed: there's
    /// nothing useful for a caller to do with that at the call site.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Non-blocking. Intended to be called once per UI frame.
    pub fn try_recv_event(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

/// What a spawned mutation (`LoadProject`/`Validate`/`Save`) reports back
/// to the actor loop. Sent from a `spawn_blocking` closure, not `.await`ed
/// inline — see README's "Why not just `Arc<Mutex<ProjectState>>`".
struct Completion {
    request: RequestId,
    /// The state to swap back in. `None` only for a failed `LoadProject`
    /// with no previously loaded project to restore (state stays absent).
    state: Option<ProjectState>,
    /// Set only by a successful `LoadProject` — the actor's own
    /// bookkeeping of where to `Save` back to.
    project_path: Option<PathBuf>,
    outcome: Outcome,
}

struct Actor<F, G> {
    state: Option<ProjectState>,
    project_path: Option<PathBuf>,
    /// See README's "Exactly one mutation at a time": set for the entire
    /// duration of a spawned `LoadProject`/`Validate`/`Save`, during which
    /// every other command — mutating or read-only — queues instead of
    /// dispatching. Cheap inline mutations (`AddRequirement`, ...) and
    /// spawned reads never set this: they complete within one `dispatch`
    /// call (inline) or don't touch `state` at all (reads), so there's
    /// nothing for a concurrent command to race with them on.
    mutation_in_flight: bool,
    queue: VecDeque<Command>,
    events: mpsc::UnboundedSender<Event>,
    completions: mpsc::UnboundedSender<Completion>,
    /// Never `Arc`-shared — cloned into each spawned `spawn_blocking`
    /// closure that needs it. Real production values (`StdFilesystem`,
    /// `SystemGit`) are zero-sized `Copy` structs, so this is free; a
    /// test's fake only needs to be cheap to clone, not literally free.
    fs: F,
    git: G,
    /// Pre-mutation `state` snapshots for `Undo` — see
    /// `push_undo_snapshot`. Bounded (`UNDO_STACK_CAPACITY`) so a long
    /// session's memory doesn't grow without limit; a `VecDeque` so the
    /// oldest entry can be dropped from the front once full.
    undo_stack: VecDeque<ProjectState>,
    /// States displaced by `Undo`, restorable by `Redo` — cleared
    /// whenever a new mutation is pushed onto `undo_stack`, the usual
    /// "a fresh edit invalidates redo history" rule. Not itself capped:
    /// bounded by `undo_stack`'s own cap indirectly (can never hold more
    /// entries than `Undo` has been called), so a second explicit cap
    /// would just be redundant bookkeeping.
    redo_stack: Vec<ProjectState>,
}

/// See `Actor::undo_stack`'s own doc comment. Not a number anyone asked
/// for — a judgment call bounding memory, same spirit as `gui-ui`'s zoom
/// range.
const UNDO_STACK_CAPACITY: usize = 50;

async fn run_actor<F, G>(
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    fs: F,
    git: G,
) where
    F: Filesystem + Clone + Send + Sync + 'static,
    G: Git + RemoteGit + Clone + Send + Sync + 'static,
{
    let (completions_tx, mut completions_rx) = mpsc::unbounded_channel();
    let mut actor = Actor {
        state: None,
        project_path: None,
        mutation_in_flight: false,
        queue: VecDeque::new(),
        events,
        completions: completions_tx,
        fs,
        git,
        undo_stack: VecDeque::new(),
        redo_stack: Vec::new(),
    };

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    None => break, // gui-ui's CoreHandle (and its sender) was dropped.
                    Some(Command::Shutdown) => break,
                    Some(command) => actor.dispatch(command),
                }
            }
            Some(completion) = completions_rx.recv() => {
                actor.apply_completion(completion);
            }
        }
    }
}

impl<F, G> Actor<F, G>
where
    F: Filesystem + Clone + Send + Sync + 'static,
    G: Git + RemoteGit + Clone + Send + Sync + 'static,
{
    fn dispatch(&mut self, command: Command) {
        // The whole exclusion policy in one check: while a mutation is
        // checked out, every other command queues, full stop — see
        // README's "Exactly one mutation at a time".
        if self.mutation_in_flight {
            self.queue.push_back(command);
            return;
        }

        match command {
            Command::LoadProject { path, request } => self.spawn_load_project(path, request),
            Command::Validate { request } => self.spawn_validate(request),
            Command::Save { request } => self.spawn_save(request),
            Command::SaveAs { path, request } => self.spawn_save_as(path, request),
            Command::NewProject { name, request } => self.new_project(name, request),
            Command::Undo { request } => self.undo(request),
            Command::Redo { request } => self.redo(request),
            Command::AddRequirement {
                module,
                name,
                requirement,
                request,
            } => self.add_requirement(module, name, requirement, request),
            Command::UpdateRequirement {
                target,
                requirement,
                request,
            } => self.update_requirement(target, requirement, request),
            Command::RefreshStaleTestReferences { target, request } => {
                self.refresh_stale_test_references(target, request)
            }
            Command::RemoveRequirement { target, request } => self.remove_requirement(target, request),
            Command::AddTest {
                module,
                name,
                test,
                request,
            } => self.add_test(module, name, test, request),
            Command::UpdateTest { target, test, request } => self.update_test(target, test, request),
            Command::RemoveTest { target, request } => self.remove_test(target, request),
            Command::AddResult {
                requirement,
                name,
                result,
                request,
            } => self.add_result(requirement, name, result, request),
            Command::UpdateResult { target, result, request } => self.update_result(target, result, request),
            Command::RemoveResult { target, request } => self.remove_result(target, request),
            Command::RefreshStaleResultReference { target, request } => {
                self.refresh_stale_result_reference(target, request)
            }
            Command::AddModule { module, name, request } => self.add_module(module, name, request),
            Command::RemoveModule { target, request } => self.remove_module(target, request),
            Command::RenameModule {
                target,
                new_name,
                reference_actions,
                request,
            } => self.rename_module(target, new_name, reference_actions, request),
            Command::RenameProject { new_name, request } => self.rename_project(new_name, request),
            Command::AddAttachment { module, path, request } => self.add_attachment(module, path, request),
            Command::RemoveAttachment { module, path, request } => self.remove_attachment(module, path, request),
            Command::AddTemplate { module, path, request } => self.add_template(module, path, request),
            Command::RemoveTemplate { module, path, request } => self.remove_template(module, path, request),
            Command::AddRequirementAttachment { target, path, request } => {
                self.add_requirement_attachment(target, path, request)
            }
            Command::RemoveRequirementAttachment { target, path, request } => {
                self.remove_requirement_attachment(target, path, request)
            }
            Command::AddTestAttachment { target, path, request } => self.add_test_attachment(target, path, request),
            Command::RemoveTestAttachment { target, path, request } => {
                self.remove_test_attachment(target, path, request)
            }
            Command::AddTestTemplateFile { target, path, request } => {
                self.add_test_template_file(target, path, request)
            }
            Command::RemoveTestTemplateFile { target, path, request } => {
                self.remove_test_template_file(target, path, request)
            }
            Command::AddResultAttachment { target, path, request } => {
                self.add_result_attachment(target, path, request)
            }
            Command::RemoveResultAttachment { target, path, request } => {
                self.remove_result_attachment(target, path, request)
            }
            Command::GetEntryDetail { target, request } => {
                self.spawn_read(request, move |state| get_entry_detail(&state, &target))
            }
            Command::GetRequirementMetStatus { target, request } => {
                self.spawn_read(request, move |state| get_requirement_met_status(&state, &target))
            }
            Command::GetResultReferenceIsStale { target, request } => {
                self.spawn_read(request, move |state| get_result_reference_is_stale(&state, &target))
            }
            Command::DependencyChain { target, request } => {
                self.spawn_read(request, move |state| dependency_chain(&state, &target))
            }
            Command::GetModulePools { module, request } => {
                self.spawn_read(request, move |state| get_module_pools(&state, &module))
            }
            Command::GetModuleSummary { module, request } => {
                self.spawn_read(request, move |state| get_module_summary(&state, &module))
            }
            Command::ResolveLocalCommit { target, kind, request } => self.spawn_resolve_local_commit(target, kind, request),
            Command::ResolveRemoteCommit { url, path, request } => self.spawn_resolve_remote_commit(url, path, request),
            Command::GetChangedFiles { request } => self.spawn_get_changed_files(request),
            Command::GetDiff { path, request } => self.spawn_get_diff(path, request),
            Command::GetCommitLog { target, request } => self.spawn_get_commit_log(target, request),
            Command::GetCommitFiles { target, commit, request } => {
                self.spawn_get_commit_files(target, commit, request)
            }
            Command::GetCommitFileDiff {
                target,
                commit,
                path,
                request,
            } => self.spawn_get_commit_file_diff(target, commit, path, request),
            Command::CommitAll { message, request } => self.spawn_commit_all(message, request),
            Command::Push { request } => self.spawn_push(request),
            Command::GetUnpushedCommits { request } => self.spawn_get_unpushed_commits(request),
            Command::FindReferences { target, request } => {
                self.spawn_read(request, move |state| find_references(&state, &target))
            }
            Command::RepairReferences {
                old_target,
                new_target,
                actions,
                request,
            } => self.repair_references(old_target, new_target, actions, request),
            // `run_actor`'s own `select!` loop intercepts `Shutdown` with a
            // `break` before ever calling `dispatch`, and `dispatch` never
            // queues a command it wouldn't otherwise have matched, so
            // `drain_queue`'s redispatch can't reach this either — but
            // that's proven by reading two other functions, not by
            // anything local to this match, so a stray `Shutdown` here is
            // ignored rather than taking down the actor's whole task.
            Command::Shutdown => {}
        }
    }

    /// A demoted-to-`Draft` project loses its resolved-reference data —
    /// expected, not a bug, per README's "Project state: alternating
    /// Draft/Validated, one owner": `logical` guarantees an edited project
    /// can't stay validated without being re-checked anyway.
    fn ensure_draft(&mut self) {
        if let Some(ProjectState::Validated(_)) = &self.state {
            let Some(ProjectState::Validated(validated)) = self.state.take() else {
                unreachable!()
            };
            self.state = Some(ProjectState::Draft(validated.into_draft()));
        }
    }

    /// Ensures `self.state` is `Draft` (demoting from `Validated` via
    /// `ensure_draft()` first if needed) and returns `&mut ProjectDraft` —
    /// or, if no project is loaded at all, completes `request` with
    /// `Outcome::NoProjectLoaded` on the caller's behalf and returns
    /// `None`. Every mutating command handler that needs to edit a `Draft`
    /// in place goes through this one accessor instead of separately
    /// hand-checking `self.state.is_none()`, calling `ensure_draft()`, and
    /// re-deriving "so this is `Some(Draft)`" via a `let-else` `.expect()`
    /// on the next line — the single point where that invariant is
    /// established is now the same single point where it's consumed,
    /// instead of the two being copied in lockstep across many call sites.
    fn draft_mut(&mut self, request: RequestId) -> Option<&mut logical::draft::ProjectDraft> {
        if self.state.is_none() {
            self.complete(request, Outcome::NoProjectLoaded);
            return None;
        }
        self.ensure_draft();
        let Some(ProjectState::Draft(draft)) = self.state.as_mut() else {
            unreachable!("ensure_draft leaves state as Draft")
        };
        Some(draft)
    }

    /// Moves `self.state`'s `Draft` out by value — the owned counterpart
    /// to `draft_mut`, for call sites that need to hand the draft to a
    /// `spawn_blocking` closure rather than mutate it in place. Callers
    /// must have already established `self.state` is `Some(Draft)`
    /// (typically via a prior `draft_mut` call in the same function).
    fn take_draft(&mut self) -> logical::draft::ProjectDraft {
        let Some(ProjectState::Draft(draft)) = self.state.take() else {
            unreachable!("caller already confirmed Some(Draft)")
        };
        draft
    }

    fn complete(&self, request: RequestId, outcome: Outcome) {
        let _ = self.events.send(Event::Completed { request, outcome });
    }

    fn push_tree_changed(&self) {
        if let Some(state) = &self.state {
            let snapshot = build_tree_snapshot(state, !self.undo_stack.is_empty(), !self.redo_stack.is_empty());
            let _ = self.events.send(Event::TreeChanged(snapshot));
        }
    }

    /// Checkpoints `state` onto `undo_stack` and clears `redo_stack` (a
    /// fresh edit invalidates whatever could have been redone). Called
    /// from `mutate_module` — the one inline-mutation entry point
    /// without a uniform "did this actually change anything" signal to
    /// gate on (its `f` returns a whole `Outcome`, shaped differently per
    /// command) — right after confirming a project is loaded, before the
    /// mutation runs. Unconditional as a result: pushed even for an
    /// attempt that turns out to fail or no-op (module not found,
    /// duplicate name, ...), same "harmless, not worth special-casing"
    /// call `mutate_module` itself already makes about firing
    /// `TreeChanged` on failure too — undoing to a snapshot identical to
    /// the current state is a no-op, just a slightly wasted stack slot,
    /// not an incorrect one. `remove_from_module`/`update_in_module`/
    /// `add_local_pool`/`remove_local_pool` all *do* have a "changed"
    /// signal already (they only fire `TreeChanged` when it's true), so
    /// they use the more precise `snapshot_state`/`commit_undo_snapshot`
    /// pair below instead, gated on that same signal.
    fn push_undo_snapshot(&mut self) {
        let Some(state) = &self.state else {
            return;
        };
        if self.undo_stack.len() >= UNDO_STACK_CAPACITY {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(state.clone());
        self.redo_stack.clear();
    }

    /// Clones `state` *before* a mutation that has its own "did anything
    /// actually change" signal, without committing it to `undo_stack`
    /// yet — the caller decides whether to `commit_undo_snapshot` it
    /// once it knows whether the mutation actually did anything, so a
    /// true no-op (module not found, entry not found, ...) doesn't waste
    /// a stack slot the way `push_undo_snapshot`'s unconditional cousin
    /// accepts for `mutate_module`.
    fn snapshot_state(&self) -> Option<ProjectState> {
        self.state.clone()
    }

    /// The other half of `snapshot_state` — actually pushes it onto
    /// `undo_stack` (capped, same as `push_undo_snapshot`) and clears
    /// `redo_stack`. Only called when the caller's own mutation actually
    /// changed something.
    fn commit_undo_snapshot(&mut self, snapshot: ProjectState) {
        if self.undo_stack.len() >= UNDO_STACK_CAPACITY {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(snapshot);
        self.redo_stack.clear();
    }

    fn undo(&mut self, request: RequestId) {
        let Some(previous) = self.undo_stack.pop_back() else {
            self.complete(request, Outcome::Undo(Err(UndoError::NothingToUndo)));
            return;
        };
        if let Some(current) = self.state.take() {
            self.redo_stack.push(current);
        }
        self.state = Some(previous);
        self.complete(request, Outcome::Undo(Ok(())));
        self.push_tree_changed();
    }

    fn redo(&mut self, request: RequestId) {
        let Some(next) = self.redo_stack.pop() else {
            self.complete(request, Outcome::Redo(Err(RedoError::NothingToRedo)));
            return;
        };
        if let Some(current) = self.state.take() {
            self.undo_stack.push_back(current);
        }
        self.state = Some(next);
        self.complete(request, Outcome::Redo(Ok(())));
        self.push_tree_changed();
    }

    /// Shared shape for every `add_*` command: resolve the target module
    /// (demoting a `Validated` project to `Draft` first if needed), then
    /// hand it to `f`. `no_module_outcome` covers the one failure `f`
    /// itself can't report — the module not existing at all. Always fires
    /// `TreeChanged`, even on failure — matches `add_requirement`'s
    /// original behavior before this was generalized; harmless (gui-ui
    /// just re-renders an unchanged tree), not worth special-casing.
    fn mutate_module(
        &mut self,
        request: RequestId,
        module: &[disk::EntryName],
        no_module_outcome: impl FnOnce() -> Outcome,
        f: impl FnOnce(&mut logical::draft::ModuleDraft) -> Outcome,
    ) {
        self.push_undo_snapshot();
        let Some(draft) = self.draft_mut(request) else { return };
        let outcome = match resolve_module_mut(&mut draft.tree, module) {
            None => no_module_outcome(),
            Some(target_module) => f(target_module),
        };
        self.complete(request, outcome);
        self.push_tree_changed();
    }

    /// Shared shape for every `remove_*` command targeting a
    /// requirement/test/result by `LogicalPath` — resolve its containing
    /// module, then hand it to `f`. Unlike `mutate_module`, only fires
    /// `TreeChanged` when something actually changed, since a `remove_*`
    /// miss (module or entry not found) is a true no-op, not just an
    /// error report.
    fn remove_from_module(
        &mut self,
        request: RequestId,
        module: &[disk::EntryName],
        f: impl FnOnce(&mut logical::draft::ModuleDraft) -> bool,
        outcome: impl FnOnce(bool) -> Outcome,
    ) {
        let undo_snapshot = self.snapshot_state();
        let Some(draft) = self.draft_mut(request) else { return };
        let removed = match resolve_module_mut(&mut draft.tree, module) {
            None => false,
            Some(target_module) => f(target_module),
        };
        self.complete(request, outcome(removed));
        if removed {
            if let Some(undo_snapshot) = undo_snapshot {
                self.commit_undo_snapshot(undo_snapshot);
            }
            self.push_tree_changed();
        }
    }

    /// Shared shape for every `update_*` command: resolve the target
    /// module, then hand it to `f` to replace the named entry — `f` is
    /// responsible for the `NotFound` check, typically by delegating to a
    /// `logical`-level `update_*` method (`ModuleDraft::update_requirement`/
    /// `update_test`, `RequirementDraft::update_result`) that owns it. Only
    /// fires `TreeChanged` when `f` actually changed something, same
    /// reasoning as `remove_from_module`.
    fn update_in_module(
        &mut self,
        request: RequestId,
        module: &[disk::EntryName],
        f: impl FnOnce(&mut logical::draft::ModuleDraft) -> Result<(), UpdateChildError>,
        outcome: impl FnOnce(Result<(), UpdateChildError>) -> Outcome,
    ) {
        let undo_snapshot = self.snapshot_state();
        let Some(draft) = self.draft_mut(request) else { return };
        let result = match resolve_module_mut(&mut draft.tree, module) {
            None => Err(UpdateChildError::ModuleNotFound),
            Some(target_module) => f(target_module),
        };
        let changed = result.is_ok();
        self.complete(request, outcome(result));
        if changed {
            if let Some(undo_snapshot) = undo_snapshot {
                self.commit_undo_snapshot(undo_snapshot);
            }
            self.push_tree_changed();
        }
    }

    fn add_requirement(
        &mut self,
        module: Vec<disk::EntryName>,
        name: disk::EntryName,
        requirement: Box<logical::draft::RequirementDraft>,
        request: RequestId,
    ) {
        self.mutate_module(
            request,
            &module,
            || Outcome::AddRequirement(Err(AddChildError::ModuleNotFound)),
            move |target_module| {
                Outcome::AddRequirement(
                    target_module
                        .add_requirement(name.as_str(), *requirement)
                        .map_err(AddChildError::Add),
                )
            },
        );
    }

    fn update_requirement(
        &mut self,
        target: logical::LogicalPath,
        requirement: Box<logical::draft::RequirementDraft>,
        request: RequestId,
    ) {
        let logical::LogicalPath { modules, name } = target;
        self.update_in_module(
            request,
            &modules,
            move |module| module.update_requirement(&name, *requirement).map_err(UpdateChildError::from),
            Outcome::UpdateRequirement,
        );
    }

    /// See `Command::RefreshStaleTestReferences`'s own doc comment. Reads
    /// `target`'s current test commits from the still-`Validated` project
    /// *before* mutating it in place (which immediately demotes to
    /// `Draft` — there'd be nothing left to compute "current" commits
    /// from once that's happened) — the same shape `update_in_module`
    /// uses, just inlined rather than delegated to it, since a successful
    /// fix here doesn't complete the request right away: it goes on to
    /// revalidate first (see below).
    fn refresh_stale_test_references(&mut self, target: logical::LogicalPath, request: RequestId) {
        let Some(ProjectState::Validated(validated)) = &self.state else {
            self.complete(
                request,
                Outcome::RefreshStaleTestReferences(Err(RefreshStaleTestReferencesError::NotValidated)),
            );
            return;
        };
        let Some(refreshed_tests) = validated.refreshed_test_references(&target) else {
            self.complete(
                request,
                Outcome::RefreshStaleTestReferences(Err(RefreshStaleTestReferencesError::Update(
                    UpdateChildError::NotFound,
                ))),
            );
            return;
        };

        let logical::LogicalPath { modules, name } = target;
        let undo_snapshot = self.snapshot_state();
        let Some(draft) = self.draft_mut(request) else { return };
        let result = match resolve_module_mut(&mut draft.tree, &modules) {
            None => Err(UpdateChildError::ModuleNotFound),
            Some(target_module) => match target_module.requirements.get_mut(&name) {
                Some(requirement) => {
                    requirement.tests = refreshed_tests;
                    Ok(())
                }
                None => Err(UpdateChildError::NotFound),
            },
        };
        if let Err(err) = result {
            self.complete(
                request,
                Outcome::RefreshStaleTestReferences(Err(RefreshStaleTestReferencesError::Update(err))),
            );
            self.push_tree_changed();
            return;
        }
        if let Some(undo_snapshot) = undo_snapshot {
            self.commit_undo_snapshot(undo_snapshot);
        }

        // The fix above is itself an edit, so (same as any other) it just
        // demoted the project back to `Draft`. Left there, every
        // requirement's `met_status` would read `Unvalidated` until the
        // user separately hit Validate — which reads as this button
        // having erased the validation results for the whole project
        // rather than just fixed one reference. So immediately revalidate,
        // same as a manual `Validate` would, instead of leaving that
        // demotion for the user to notice and undo themselves.
        let draft = self.take_draft();
        self.mutation_in_flight = true;
        let completions = self.completions.clone();
        let remote_git = self.git.clone();
        tokio::task::spawn_blocking(move || {
            // Same "clone first so a failed validation still leaves an
            // editable draft behind" reasoning as `spawn_validate`.
            let restore = draft.clone();
            let state = match logical::validate::validate(draft, &remote_git) {
                Ok(validated) => ProjectState::Validated(validated),
                Err(_) => ProjectState::Draft(restore),
            };
            let _ = completions.send(Completion {
                request,
                state: Some(state),
                project_path: None,
                outcome: Outcome::RefreshStaleTestReferences(Ok(())),
            });
        });
    }

    /// See `Command::RefreshStaleResultReference`'s own doc comment — same
    /// "read the still-`Validated` project's current commits before
    /// mutating it in place, then immediately revalidate" shape as
    /// `refresh_stale_test_references`, just targeting a result's own
    /// `requirement_commit`/`test_commit` instead of a requirement's
    /// `tests` list.
    fn refresh_stale_result_reference(&mut self, target: logical::ResultPath, request: RequestId) {
        let Some(ProjectState::Validated(validated)) = &self.state else {
            self.complete(
                request,
                Outcome::RefreshStaleResultReference(Err(RefreshStaleTestReferencesError::NotValidated)),
            );
            return;
        };
        let Some(refreshed_result) = validated.refreshed_result(&target) else {
            self.complete(
                request,
                Outcome::RefreshStaleResultReference(Err(RefreshStaleTestReferencesError::Update(
                    UpdateChildError::NotFound,
                ))),
            );
            return;
        };

        let logical::ResultPath { requirement, name } = target;
        let logical::LogicalPath { modules, name: requirement_name } = requirement;
        let undo_snapshot = self.snapshot_state();
        let Some(draft) = self.draft_mut(request) else { return };
        let result = match resolve_module_mut(&mut draft.tree, &modules) {
            None => Err(UpdateChildError::ModuleNotFound),
            Some(target_module) => match target_module.requirements.get_mut(&requirement_name) {
                Some(requirement) => {
                    if let std::collections::btree_map::Entry::Occupied(mut entry) = requirement.results.entry(name)
                    {
                        entry.insert(refreshed_result);
                        Ok(())
                    } else {
                        Err(UpdateChildError::NotFound)
                    }
                }
                None => Err(UpdateChildError::NotFound),
            },
        };
        if let Err(err) = result {
            self.complete(
                request,
                Outcome::RefreshStaleResultReference(Err(RefreshStaleTestReferencesError::Update(err))),
            );
            self.push_tree_changed();
            return;
        }
        if let Some(undo_snapshot) = undo_snapshot {
            self.commit_undo_snapshot(undo_snapshot);
        }

        // Same "an edit just demoted to `Draft`; revalidate immediately so
        // this doesn't read as having wiped the whole project's validation
        // results" reasoning as `refresh_stale_test_references`.
        let draft = self.take_draft();
        self.mutation_in_flight = true;
        let completions = self.completions.clone();
        let remote_git = self.git.clone();
        tokio::task::spawn_blocking(move || {
            let restore = draft.clone();
            let state = match logical::validate::validate(draft, &remote_git) {
                Ok(validated) => ProjectState::Validated(validated),
                Err(_) => ProjectState::Draft(restore),
            };
            let _ = completions.send(Completion {
                request,
                state: Some(state),
                project_path: None,
                outcome: Outcome::RefreshStaleResultReference(Ok(())),
            });
        });
    }

    fn remove_requirement(&mut self, target: logical::LogicalPath, request: RequestId) {
        let logical::LogicalPath { modules, name } = target;
        self.remove_from_module(
            request,
            &modules,
            move |module| module.remove_requirement(name.as_str()).is_some(),
            Outcome::RemoveRequirement,
        );
    }

    fn add_test(
        &mut self,
        module: Vec<disk::EntryName>,
        name: disk::EntryName,
        test: Box<logical::draft::TestDraft>,
        request: RequestId,
    ) {
        self.mutate_module(
            request,
            &module,
            || Outcome::AddTest(Err(AddChildError::ModuleNotFound)),
            move |target_module| {
                Outcome::AddTest(target_module.add_test(name.as_str(), *test).map_err(AddChildError::Add))
            },
        );
    }

    fn update_test(
        &mut self,
        target: logical::LogicalPath,
        test: Box<logical::draft::TestDraft>,
        request: RequestId,
    ) {
        let logical::LogicalPath { modules, name } = target;
        self.update_in_module(
            request,
            &modules,
            move |module| module.update_test(&name, *test).map_err(UpdateChildError::from),
            Outcome::UpdateTest,
        );
    }

    fn remove_test(&mut self, target: logical::LogicalPath, request: RequestId) {
        let logical::LogicalPath { modules, name } = target;
        self.remove_from_module(
            request,
            &modules,
            move |module| module.remove_test(name.as_str()).is_some(),
            Outcome::RemoveTest,
        );
    }

    fn add_result(
        &mut self,
        requirement: logical::LogicalPath,
        name: disk::EntryName,
        result: Box<logical::draft::ResultDraft>,
        request: RequestId,
    ) {
        let logical::LogicalPath { modules, name: requirement_name } = requirement;
        self.mutate_module(
            request,
            &modules,
            || Outcome::AddResult(Err(AddChildError::ModuleNotFound)),
            move |target_module| {
                let Some(requirement) = target_module.requirements.get_mut(&requirement_name) else {
                    return Outcome::AddResult(Err(AddChildError::RequirementNotFound));
                };
                Outcome::AddResult(
                    requirement
                        .add_result(name.as_str(), *result)
                        .map_err(AddChildError::Add),
                )
            },
        );
    }

    fn update_result(
        &mut self,
        target: logical::ResultPath,
        result: Box<logical::draft::ResultDraft>,
        request: RequestId,
    ) {
        let logical::ResultPath { requirement, name } = target;
        let logical::LogicalPath { modules, name: requirement_name } = requirement;
        self.update_in_module(
            request,
            &modules,
            move |module| {
                let Some(requirement) = module.requirements.get_mut(&requirement_name) else {
                    return Err(UpdateChildError::NotFound);
                };
                requirement.update_result(&name, *result).map_err(UpdateChildError::from)
            },
            Outcome::UpdateResult,
        );
    }

    fn remove_result(&mut self, target: logical::ResultPath, request: RequestId) {
        let logical::ResultPath { requirement, name } = target;
        let logical::LogicalPath { modules, name: requirement_name } = requirement;
        self.remove_from_module(
            request,
            &modules,
            move |module| {
                module
                    .requirements
                    .get_mut(&requirement_name)
                    .map(|requirement| requirement.remove_result(name.as_str()).is_some())
                    .unwrap_or(false)
            },
            Outcome::RemoveResult,
        );
    }

    fn add_module(&mut self, module: Vec<disk::EntryName>, name: disk::EntryName, request: RequestId) {
        self.mutate_module(
            request,
            &module,
            || Outcome::AddModule(Err(AddChildError::ModuleNotFound)),
            move |target_module| Outcome::AddModule(target_module.add_module(name.as_str()).map_err(AddChildError::Add)),
        );
    }

    /// `target` is the module being removed itself, not a parent to add
    /// into — see `Command::RemoveModule`'s doc comment. Removing the
    /// project root (`target` empty) isn't a thing `logical` supports, so
    /// it's reported the same as "not found" rather than panicking on the
    /// `split_last` below.
    fn remove_module(&mut self, target: Vec<disk::EntryName>, request: RequestId) {
        let Some((name, parent)) = target.split_last() else {
            self.complete(request, Outcome::RemoveModule(false));
            return;
        };
        let name = name.clone();
        self.remove_from_module(
            request,
            parent,
            move |module| module.remove_module(name.as_str()).is_some(),
            Outcome::RemoveModule,
        );
    }

    /// Renames a module in place: `target`'s last segment names the
    /// module to rename, its parent is where the rename happens.
    /// Validates `new_name` (sanitization, collision) by routing through
    /// `ModuleDraft::add_module` itself rather than duplicating that
    /// logic — a throwaway blank module gets created there and then
    /// immediately overwritten with the real (renamed-from) content, so
    /// `add_module`'s own checks are what actually gate the rename.
    /// Unlike every other `RenameModule` step above `mutate_module`
    /// couldn't be reused here: once the rename itself succeeds,
    /// `reference_actions` needs to repair references that can live
    /// *anywhere* in the tree (not just within the renamed module's own
    /// subtree), which needs `&mut ProjectDraft` — `mutate_module` only
    /// ever hands its closure `&mut ModuleDraft` for the resolved target.
    /// So this resolves the parent module itself, directly against
    /// `draft.tree`.
    fn rename_module(
        &mut self,
        target: Vec<disk::EntryName>,
        new_name: disk::EntryName,
        reference_actions: Vec<(logical::ReferenceSite, logical::ReferenceAction)>,
        request: RequestId,
    ) {
        let Some((old_name, parent_path)) = target.split_last() else {
            self.complete(request, Outcome::RenameModule(Err(RenameModuleError::CannotRenameRoot)));
            return;
        };
        let old_name = old_name.clone();
        let parent_path = parent_path.to_vec();
        let old_path = target.clone();
        let mut new_path = parent_path.clone();
        new_path.push(new_name.clone());

        self.push_undo_snapshot();
        let Some(draft) = self.draft_mut(request) else { return };

        let rename_result = match resolve_module_mut(&mut draft.tree, &parent_path) {
            None => Err(RenameModuleError::ModuleNotFound),
            Some(parent) => {
                if !parent.modules.contains_key(&old_name) {
                    Err(RenameModuleError::NotFound)
                } else if let Err(err) = parent.add_module(new_name.as_str()) {
                    Err(RenameModuleError::Add(err))
                } else {
                    // `remove` returning `None` here would mean `old_name`
                    // vanished between the `contains_key` check above and
                    // here — impossible today (nothing else touches
                    // `parent` in between) but reported as `NotFound`
                    // instead of panicking if that ever stops being true.
                    match parent.modules.remove(&old_name) {
                        Some(renamed) => {
                            parent.modules.insert(new_name.clone(), renamed);
                            Ok(())
                        }
                        None => Err(RenameModuleError::NotFound),
                    }
                }
            }
        };

        let outcome = match rename_result {
            Ok(()) if !reference_actions.is_empty() => {
                let old_target = logical::ReferenceTarget::Module(old_path);
                let new_target = logical::ReferenceTarget::Module(new_path);
                logical::apply_reference_actions(draft, &old_target, Some(&new_target), &reference_actions)
                    .map_err(RenameModuleError::ReferenceRepair)
            }
            other => other,
        };
        self.complete(request, Outcome::RenameModule(outcome));
        self.push_tree_changed();
    }

    /// `RepairReferences`'s handler — applies `actions` (a prior
    /// `FindReferences`'s per-site user choices) to `old_target`/
    /// `new_target`, for the requirement/test recreate flow: `gui-ui`
    /// sends this as a third step once `RemoveRequirement`/`RemoveTest`
    /// then `AddRequirement`/`AddTest` have both already succeeded, so the
    /// new path already exists to repair `Repair` actions onto. Doesn't
    /// push its own undo snapshot — it's always part of a larger rename/
    /// recreate sequence `gui-ui` already snapshotted around.
    fn repair_references(
        &mut self,
        old_target: logical::ReferenceTarget,
        new_target: Option<logical::ReferenceTarget>,
        actions: Vec<(logical::ReferenceSite, logical::ReferenceAction)>,
        request: RequestId,
    ) {
        let Some(draft) = self.draft_mut(request) else { return };
        let result = logical::apply_reference_actions(draft, &old_target, new_target.as_ref(), &actions);
        self.complete(request, Outcome::RepairReferences(result));
        self.push_tree_changed();
    }

    /// `RenameModule`'s root-only counterpart — see `Command::RenameProject`'s
    /// own doc comment. Uses `snapshot_state`/`commit_undo_snapshot` (only
    /// commit on success) rather than `mutate_module`'s unconditional
    /// `push_undo_snapshot`, since there's a real "did this fail" signal
    /// here (an empty name) and no module resolution involved at all.
    fn rename_project(&mut self, new_name: String, request: RequestId) {
        if self.state.is_none() {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        }
        if new_name.trim().is_empty() {
            self.complete(request, Outcome::RenameProject(Err(RenameProjectError::EmptyName)));
            return;
        }
        let undo_snapshot = self.snapshot_state();
        let Some(draft) = self.draft_mut(request) else { return };
        draft.definition.name = new_name;
        self.complete(request, Outcome::RenameProject(Ok(())));
        if let Some(undo_snapshot) = undo_snapshot {
            self.commit_undo_snapshot(undo_snapshot);
        }
        self.push_tree_changed();
    }

    fn add_attachment(&mut self, module: Vec<disk::EntryName>, path: std::path::PathBuf, request: RequestId) {
        self.mutate_module(
            request,
            &module,
            || Outcome::AddAttachment(Err(AddPoolChildError::ModuleNotFound)),
            move |target_module| {
                Outcome::AddAttachment(target_module.add_attachment(&path).map_err(AddPoolChildError::Add))
            },
        );
    }

    fn remove_attachment(&mut self, module: Vec<disk::EntryName>, path: std::path::PathBuf, request: RequestId) {
        self.remove_from_module(
            request,
            &module,
            move |target_module| target_module.remove_attachment(&path),
            Outcome::RemoveAttachment,
        );
    }

    fn add_template(&mut self, module: Vec<disk::EntryName>, path: std::path::PathBuf, request: RequestId) {
        self.mutate_module(
            request,
            &module,
            || Outcome::AddTemplate(Err(AddPoolChildError::ModuleNotFound)),
            move |target_module| {
                Outcome::AddTemplate(target_module.add_template(&path).map_err(AddPoolChildError::Add))
            },
        );
    }

    fn remove_template(&mut self, module: Vec<disk::EntryName>, path: std::path::PathBuf, request: RequestId) {
        self.remove_from_module(
            request,
            &module,
            move |target_module| target_module.remove_template(&path),
            Outcome::RemoveTemplate,
        );
    }

    /// Shared shape for every local-pool `add_*` command (a requirement/
    /// test's local attachments, a test's local template files, a
    /// result's local attachments): resolve the target's module, look up
    /// the target entry itself within it (`get_entry` — the extra step
    /// `mutate_module` doesn't need, since a local pool belongs to one
    /// entry, not the module as a whole), then apply `f`. `get_entry`
    /// takes the whole resolved module rather than a fixed `(module,
    /// name)` pair so a result's caller can chain one extra hop (module ->
    /// requirement -> its nested `results`) that a requirement/test's
    /// single-hop lookup doesn't need.
    fn add_local_pool<T>(
        &mut self,
        request: RequestId,
        module: &[disk::EntryName],
        get_entry: impl for<'a> FnOnce(&'a mut logical::draft::ModuleDraft) -> Option<&'a mut T>,
        f: impl FnOnce(&mut T) -> Result<(), AddPoolFileError>,
        outcome: impl FnOnce(Result<(), AddLocalPoolError>) -> Outcome,
    ) {
        let undo_snapshot = self.snapshot_state();
        let Some(draft) = self.draft_mut(request) else { return };
        let result = match resolve_module_mut(&mut draft.tree, module) {
            None => Err(AddLocalPoolError::ModuleNotFound),
            Some(module) => match get_entry(module) {
                None => Err(AddLocalPoolError::EntryNotFound),
                Some(entry) => f(entry).map_err(AddLocalPoolError::Add),
            },
        };
        let changed = result.is_ok();
        self.complete(request, outcome(result));
        if changed {
            if let Some(undo_snapshot) = undo_snapshot {
                self.commit_undo_snapshot(undo_snapshot);
            }
            self.push_tree_changed();
        }
    }

    /// The `remove_*` counterpart to `add_local_pool` — `false` (never an
    /// error) covers module-not-found, entry-not-found, and "that path
    /// wasn't in the pool," same "remove never fails, it's just
    /// `Option`/`bool`-shaped" spirit as `remove_from_module`.
    fn remove_local_pool<T>(
        &mut self,
        request: RequestId,
        module: &[disk::EntryName],
        get_entry: impl for<'a> FnOnce(&'a mut logical::draft::ModuleDraft) -> Option<&'a mut T>,
        f: impl FnOnce(&mut T) -> bool,
        outcome: impl FnOnce(bool) -> Outcome,
    ) {
        let undo_snapshot = self.snapshot_state();
        let Some(draft) = self.draft_mut(request) else { return };
        let removed = match resolve_module_mut(&mut draft.tree, module) {
            None => false,
            Some(module) => match get_entry(module) {
                None => false,
                Some(entry) => f(entry),
            },
        };
        self.complete(request, outcome(removed));
        if removed {
            if let Some(undo_snapshot) = undo_snapshot {
                self.commit_undo_snapshot(undo_snapshot);
            }
            self.push_tree_changed();
        }
    }

    fn add_requirement_attachment(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.add_local_pool(
            request,
            &target.modules,
            move |module| module.requirements.get_mut(&target.name),
            move |requirement: &mut logical::draft::RequirementDraft| requirement.add_attachment(&path),
            Outcome::AddRequirementAttachment,
        );
    }

    fn remove_requirement_attachment(
        &mut self,
        target: logical::LogicalPath,
        path: std::path::PathBuf,
        request: RequestId,
    ) {
        self.remove_local_pool(
            request,
            &target.modules,
            move |module| module.requirements.get_mut(&target.name),
            move |requirement: &mut logical::draft::RequirementDraft| requirement.remove_attachment(&path),
            Outcome::RemoveRequirementAttachment,
        );
    }

    fn add_test_attachment(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.add_local_pool(
            request,
            &target.modules,
            move |module| module.tests.get_mut(&target.name),
            move |test: &mut logical::draft::TestDraft| test.add_attachment(&path),
            Outcome::AddTestAttachment,
        );
    }

    fn remove_test_attachment(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.remove_local_pool(
            request,
            &target.modules,
            move |module| module.tests.get_mut(&target.name),
            move |test: &mut logical::draft::TestDraft| test.remove_attachment(&path),
            Outcome::RemoveTestAttachment,
        );
    }

    fn add_test_template_file(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.add_local_pool(
            request,
            &target.modules,
            move |module| module.tests.get_mut(&target.name),
            move |test: &mut logical::draft::TestDraft| test.add_template_file(&path),
            Outcome::AddTestTemplateFile,
        );
    }

    fn remove_test_template_file(
        &mut self,
        target: logical::LogicalPath,
        path: std::path::PathBuf,
        request: RequestId,
    ) {
        self.remove_local_pool(
            request,
            &target.modules,
            move |module| module.tests.get_mut(&target.name),
            move |test: &mut logical::draft::TestDraft| test.remove_template_file(&path),
            Outcome::RemoveTestTemplateFile,
        );
    }

    fn add_result_attachment(&mut self, target: logical::ResultPath, path: std::path::PathBuf, request: RequestId) {
        self.add_local_pool(
            request,
            &target.requirement.modules,
            move |module| {
                module
                    .requirements
                    .get_mut(&target.requirement.name)?
                    .results
                    .get_mut(&target.name)
            },
            move |result: &mut logical::draft::ResultDraft| result.add_attachment(&path),
            Outcome::AddResultAttachment,
        );
    }

    fn remove_result_attachment(
        &mut self,
        target: logical::ResultPath,
        path: std::path::PathBuf,
        request: RequestId,
    ) {
        self.remove_local_pool(
            request,
            &target.requirement.modules,
            move |module| {
                module
                    .requirements
                    .get_mut(&target.requirement.name)?
                    .results
                    .get_mut(&target.name)
            },
            move |result: &mut logical::draft::ResultDraft| result.remove_attachment(&path),
            Outcome::RemoveResultAttachment,
        );
    }

    /// Reads never set `mutation_in_flight` — they operate on their own
    /// `state.clone()` (never a shared reference), so any number of them
    /// can run at once without racing each other or the actor. See
    /// README's "Exactly one mutation at a time" and "`Arc`/`Mutex` are
    /// both off the table".
    fn spawn_read(&self, request: RequestId, compute: impl FnOnce(ProjectState) -> Outcome + Send + 'static) {
        let Some(state) = self.state.clone() else {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        };
        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome = compute(state);
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::ResolveLocalCommit`'s own doc comment. Touches no
    /// project state (so, unlike `spawn_load_project`, doesn't go through
    /// `Completion`/`mutation_in_flight` at all) but still shells out to
    /// `git`, so it runs on a blocking-pool thread rather than inline like
    /// `spawn_read`'s in-memory reads do.
    fn spawn_resolve_local_commit(&self, target: LogicalPath, kind: LocalCommitKind, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::ResolveLocalCommit(Err(ResolveLocalCommitError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let dir = entry_directory(&project_path, &target, kind);
            let outcome = Outcome::ResolveLocalCommit(git.commit_for_path(&dir).map_err(ResolveLocalCommitError::from));
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::ResolveRemoteCommit`'s own doc comment — no project
    /// state (or even a loaded project) involved at all, just `url`/`path`
    /// handed straight to `RemoteGit::commit_for_remote`.
    fn spawn_resolve_remote_commit(&self, url: String, path: Option<ReferencePath>, request: RequestId) {
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let path = path.map(|p| PathBuf::from(p.0));
            let outcome = Outcome::ResolveRemoteCommit(git.commit_for_remote(&url, path.as_deref()));
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::GetChangedFiles`'s own doc comment — same "no project
    /// state touched, still shells out to `git`, so it's spawned" shape as
    /// `spawn_resolve_local_commit`.
    fn spawn_get_changed_files(&self, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::GetChangedFiles(Err(GetChangedFilesError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = Outcome::GetChangedFiles(git.changed_paths(&project_path).map_err(GetChangedFilesError::from));
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::GetDiff`'s own doc comment — same shape as
    /// `spawn_get_changed_files`, against a caller-supplied `path` instead
    /// of the whole working directory.
    fn spawn_get_diff(&self, path: PathBuf, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::GetDiff(Err(GetDiffError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = Outcome::GetDiff(git.diff(&project_path, &path).map_err(GetDiffError::from));
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::GetCommitLog`'s own doc comment — same "no project
    /// state touched, still shells out to `git`, so it's spawned" shape as
    /// `spawn_get_diff`, against `commit_log_directory(&target)` and a
    /// fixed exclude set (`commit_log_excludes`) instead of a caller-
    /// supplied path.
    fn spawn_get_commit_log(&self, target: EntryPath, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::GetCommitLog(Err(GetCommitLogError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let dir = commit_log_directory(&project_path, &target);
            let excludes = commit_log_excludes(&dir, &target);
            let exclude_refs: Vec<&Path> = excludes.iter().map(PathBuf::as_path).collect();
            let outcome = Outcome::GetCommitLog(
                git.commit_log_for_path(&dir, &exclude_refs, COMMIT_LOG_LIMIT)
                    .map_err(GetCommitLogError::from),
            );
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::GetCommitFiles`'s own doc comment — same shape as
    /// `spawn_get_commit_log`, reusing the exact same `commit_log_directory`/
    /// `commit_log_excludes` helpers so the file list stays in
    /// scope-agreement with the log entry (`commit`) it's opened from.
    fn spawn_get_commit_files(&self, target: EntryPath, commit: String, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::GetCommitFiles(Err(GetCommitFilesError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let dir = commit_log_directory(&project_path, &target);
            let excludes = commit_log_excludes(&dir, &target);
            let exclude_refs: Vec<&Path> = excludes.iter().map(PathBuf::as_path).collect();
            let outcome = Outcome::GetCommitFiles(
                git.files_changed_in_commit(&dir, &commit, &exclude_refs)
                    .map_err(GetCommitFilesError::from),
            );
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::GetCommitFileDiff`'s own doc comment — same shape as
    /// `spawn_get_diff`, against `commit_log_directory(&target)` (`path` is
    /// already relative to that directory, per `GetCommitFiles`'s own
    /// contract) and `commit` instead of always diffing the working tree.
    fn spawn_get_commit_file_diff(&self, target: EntryPath, commit: String, path: PathBuf, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::GetCommitFileDiff(Err(GetCommitFileDiffError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let dir = commit_log_directory(&project_path, &target);
            let outcome = Outcome::GetCommitFileDiff(
                git.diff_for_commit(&dir, &commit, &path)
                    .map_err(GetCommitFileDiffError::from),
            );
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::CommitAll`'s own doc comment.
    fn spawn_commit_all(&self, message: String, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::CommitAll(Err(CommitAllError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = Outcome::CommitAll(git.commit_all(&project_path, &message).map_err(CommitAllError::from));
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::Push`'s own doc comment.
    fn spawn_push(&self, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(request, Outcome::Push(Err(PushError::NoProjectPath)));
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = Outcome::Push(git.push(&project_path).map_err(PushError::from));
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    /// See `Command::GetUnpushedCommits`'s own doc comment — same shape as
    /// `spawn_get_changed_files`, against `syscalls::Git::unpushed_commits`
    /// instead of `changed_paths`.
    fn spawn_get_unpushed_commits(&self, request: RequestId) {
        let Some(project_path) = self.project_path.clone() else {
            self.complete(
                request,
                Outcome::GetUnpushedCommits(Err(GetUnpushedCommitsError::NoProjectPath)),
            );
            return;
        };
        let git = self.git.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = Outcome::GetUnpushedCommits(
                git.unpushed_commits(&project_path)
                    .map_err(GetUnpushedCommitsError::from),
            );
            let _ = events.send(Event::Completed { request, outcome });
        });
    }

    fn spawn_load_project(&mut self, path: PathBuf, request: RequestId) {
        let previous_state = self.state.take();
        self.mutation_in_flight = true;
        let completions = self.completions.clone();
        let fs = self.fs.clone();
        let git = self.git.clone();
        tokio::task::spawn_blocking(move || {
            let result = disk::load_project(&fs, &git, &path);
            let completion = match result {
                Ok(on_disk) => {
                    let draft = logical::convert::import_project(on_disk);
                    Completion {
                        request,
                        state: Some(ProjectState::Draft(draft)),
                        project_path: Some(path),
                        outcome: Outcome::LoadProject(Ok(())),
                    }
                }
                Err(err) => Completion {
                    request,
                    state: previous_state,
                    project_path: None,
                    outcome: Outcome::LoadProject(Err(err)),
                },
            };
            let _ = completions.send(completion);
        });
    }

    fn spawn_validate(&mut self, request: RequestId) {
        let Some(state) = self.state.take() else {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        };
        self.mutation_in_flight = true;
        let draft = match state {
            ProjectState::Draft(draft) => draft,
            ProjectState::Validated(validated) => validated.into_draft(),
        };
        let completions = self.completions.clone();
        let remote_git = self.git.clone();
        tokio::task::spawn_blocking(move || {
            // `validate()` consumes the draft and drops it on `Err` (see
            // README's `logical::validate::validate` note) — clone first
            // so a failed validation still leaves an editable draft behind
            // instead of losing the project.
            let restore = draft.clone();
            let (state, outcome) = match logical::validate::validate(draft, &remote_git) {
                Ok(validated) => (ProjectState::Validated(validated), Outcome::Validate(Ok(()))),
                Err(errors) => (ProjectState::Draft(restore), Outcome::Validate(Err(errors))),
            };
            let _ = completions.send(Completion {
                request,
                state: Some(state),
                project_path: None,
                outcome,
            });
        });
    }

    fn spawn_save(&mut self, request: RequestId) {
        let Some(state) = self.state.take() else {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        };
        let Some(dir) = self.project_path.clone() else {
            // Nothing loaded from (or saved to) a path yet — restore state
            // as-is, nothing was checked out for real.
            self.state = Some(state);
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        };
        self.mutation_in_flight = true;
        let completions = self.completions.clone();
        let fs = self.fs.clone();
        let git = self.git.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = match &state {
                ProjectState::Validated(validated) => {
                    Outcome::Save(validated.save(&fs, &git, &dir).map_err(SaveError::Save))
                }
                ProjectState::Draft(_) => Outcome::Save(Err(SaveError::NotValidated)),
            };
            let _ = completions.send(Completion {
                request,
                state: Some(state),
                project_path: None,
                outcome,
            });
        });
    }

    /// Same shape as `spawn_save`, against a caller-supplied `path`
    /// instead of `self.project_path` — no early "nothing loaded from a
    /// path yet" bailout, since picking the path is the whole point.
    /// `project_path` only updates to `path` when the save actually
    /// succeeds (`Completion::project_path`, the same field `LoadProject`
    /// uses); a failed `SaveAs` leaves it exactly as it was, same as a
    /// failed `LoadProject` doesn't overwrite it with a half-loaded path.
    fn spawn_save_as(&mut self, path: PathBuf, request: RequestId) {
        let Some(state) = self.state.take() else {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        };
        self.mutation_in_flight = true;
        let completions = self.completions.clone();
        let fs = self.fs.clone();
        let git = self.git.clone();
        tokio::task::spawn_blocking(move || {
            let (outcome, saved_path) = match &state {
                ProjectState::Validated(validated) => match validated.save(&fs, &git, &path) {
                    Ok(()) => (Outcome::SaveAs(Ok(())), Some(path)),
                    Err(err) => (Outcome::SaveAs(Err(SaveError::Save(err))), None),
                },
                ProjectState::Draft(_) => (Outcome::SaveAs(Err(SaveError::NotValidated)), None),
            };
            let _ = completions.send(Completion {
                request,
                state: Some(state),
                project_path: saved_path,
                outcome,
            });
        });
    }

    /// Purely in-memory (see `Command::NewProject`'s doc comment) — an
    /// inline mutation like `add_requirement`, not spawned. Replaces
    /// whatever project was loaded (if any) with a blank one under
    /// `name`, and clears `project_path`: a brand new project has no
    /// on-disk home until a `SaveAs` gives it one.
    fn new_project(&mut self, name: String, request: RequestId) {
        self.state = Some(ProjectState::Draft(logical::draft::create_project(name)));
        self.project_path = None;
        // Undoing across a project switch would mean restoring a
        // previous, unrelated project's state — not what "undo" means
        // here. See `apply_completion`'s own clear on a successful
        // `LoadProject`.
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.push_tree_changed();
        self.complete(request, Outcome::NewProject);
    }

    fn apply_completion(&mut self, completion: Completion) {
        if let Some(state) = completion.state {
            self.state = Some(state);
        }
        if let Some(path) = completion.project_path {
            self.project_path = Some(path);
        }
        self.mutation_in_flight = false;

        if let Outcome::Validate(Err(errors)) = &completion.outcome {
            let _ = self.events.send(Event::ValidationFailed(errors.clone()));
        }

        // Same "a project switch invalidates undo history" reasoning as
        // `new_project`'s own clear — only on success: a failed
        // `LoadProject` leaves whatever was already loaded (if anything)
        // untouched, so its undo history is still valid.
        if matches!(completion.outcome, Outcome::LoadProject(Ok(()))) {
            self.undo_stack.clear();
            self.redo_stack.clear();
        }

        self.complete(completion.request, completion.outcome);
        self.push_tree_changed();
        self.drain_queue();
    }

    /// Re-dispatches whatever queued up while a mutation was checked out,
    /// in arrival order. Stops as soon as a queued mutation starts a new
    /// spawn (setting `mutation_in_flight` again) — anything still behind
    /// it stays queued for the next completion.
    fn drain_queue(&mut self) {
        while !self.mutation_in_flight {
            match self.queue.pop_front() {
                Some(command) => self.dispatch(command),
                None => break,
            }
        }
    }
}

/// `target`'s own directory under `project_path` — mirrors the on-disk
/// layout `disk::module::operations` reads/writes
/// (`[modules/<sub>/]*<kind_segment>/<name>`), the same shape `gui-ui`'s
/// own `absolute_reference_path` builds as a logical reference path string
/// rather than a real filesystem path. Used by `spawn_resolve_local_commit`
/// and (for the `Requirement`/`Test` cases) `commit_log_directory` below —
/// every other place gui-core touches disk paths goes through `disk`
/// itself instead of reimplementing its layout, but there's no existing
/// `disk`-level "path for this entry" function to call into here.
///
fn entry_directory(project_path: &Path, target: &LogicalPath, kind: LocalCommitKind) -> PathBuf {
    let mut dir = project_path.to_path_buf();
    for module in &target.modules {
        dir.push("modules");
        dir.push(module.as_str());
    }
    dir.push(match kind {
        LocalCommitKind::Requirement => "requirements",
        LocalCommitKind::Test => "tests",
    });
    dir.push(target.name.as_str());
    dir
}

/// The maximum number of entries `spawn_get_commit_log` asks
/// `Git::commit_log_for_path` for — a "Commit history" section is a
/// browsing aid, not a paged view, so this is a fixed cap rather than
/// something `gui-ui` gets to configure.
const COMMIT_LOG_LIMIT: usize = 50;

/// `target`'s own directory, generalizing `entry_directory` to also cover a
/// `Result` — unlike `LocalCommitKind`, `EntryPath` (`Command::GetCommitLog`'s
/// own address type) can name a result, which lives at
/// `<its requirement's directory>/results/<name>` (see `disk::result::
/// operations`) even though, unlike a requirement/test, it has no
/// `LocalCommitKind` of its own to pass to `entry_directory`.
fn commit_log_directory(project_path: &Path, target: &EntryPath) -> PathBuf {
    match target {
        EntryPath::Requirement(path) => entry_directory(project_path, path, LocalCommitKind::Requirement),
        EntryPath::Test(path) => entry_directory(project_path, path, LocalCommitKind::Test),
        EntryPath::Result(result_path) => {
            let mut dir = entry_directory(project_path, &result_path.requirement, LocalCommitKind::Requirement);
            dir.push("results");
            dir.push(result_path.name.as_str());
            dir
        }
    }
}

/// The subdirectories of `dir` a "Commit history" log excludes, per entry
/// kind — a fixed approximation of what `disk::load` itself excludes when
/// computing a requirement's/test's single "current commit" field
/// (`crates/disk/src/requirement/operations/load.rs`,
/// `crates/disk/src/test/operations/load.rs`): a requirement always
/// excludes its own `results/` (nested children, not part of its own
/// text) and, unconditionally here, `attachments/`; a test unconditionally
/// excludes `attachments/`/`template/`. `disk::load` only excludes
/// attachments/template when each entry's own `include_attachments_in_commit`/
/// `include_template_in_commit` toggle says not to include them — reading
/// that live per-entry setting would mean this stateless, `GetDiff`-style
/// read needs project state too, so it's deliberately not replicated here.
/// The log is a browsing aid, not the source of truth for staleness (which
/// stays exactly as computed today); a result has no such toggle and
/// excludes nothing.
fn commit_log_excludes(dir: &Path, target: &EntryPath) -> Vec<PathBuf> {
    match target {
        EntryPath::Requirement(_) => vec![dir.join("results"), dir.join("attachments")],
        EntryPath::Test(_) => vec![dir.join("attachments"), dir.join("template")],
        EntryPath::Result(_) => Vec::new(),
    }
}

#[cfg(test)]
mod test {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use disk::{
        DependencyReferenceKind, EntryName, LocalGitReference, ReferencePath, RemoteGitReference, TestReferenceKind,
    };
    use logical::LogicalPath;
    use logical::draft::RequirementDraft;
    use syscalls::{
        ChangedPathsError, CommitAllError, CommitForPathError, CommitForRemoteError, DiffError, PushError, StdFilesystem,
    };

    use logical::draft::AddNamedChildError;

    use crate::{
        AddPoolFileError, EntryDetail, EntryPath, GetChangedFilesError, GetCommitLogError, GetDiffError,
        RefreshStaleTestReferencesError, RequirementMetStatus, TestUnmetReason, TreeSnapshot, UnmetReason,
    };

    use super::*;

    /// Deterministic, no real `git` shell-out — same role as `disk`'s own
    /// `test_support::FixedGit` (not `pub` outside that crate), extended to
    /// also implement `RemoteGit` since `Actor`'s `G` bound needs both.
    #[derive(Debug, Clone, Copy, Default)]
    struct FixedGit;

    impl syscalls::Git for FixedGit {
        fn commit_for_path_excluding(&self, _path: &Path, _excludes: &[&Path]) -> Result<String, CommitForPathError> {
            Ok("deadbeef".to_string())
        }

        fn changed_paths(&self, _dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
            Ok(vec![PathBuf::from("root.txt"), PathBuf::from("sub/file.txt")])
        }

        fn commit_all(&self, _dir: &Path, _message: &str) -> Result<(), CommitAllError> {
            Ok(())
        }

        fn diff(&self, _dir: &Path, path: &Path) -> Result<String, DiffError> {
            Ok(format!("diff for {}", path.display()))
        }

        fn push(&self, _dir: &Path) -> Result<String, PushError> {
            Ok("pushed".to_string())
        }

        fn unpushed_commits(&self, _dir: &Path) -> Result<Vec<syscalls::CommitInfo>, syscalls::UnpushedCommitsError> {
            Ok(vec![syscalls::CommitInfo {
                hash: "deadbeef".to_string(),
                subject: "a local commit".to_string(),
                date: "2024-01-01".to_string(),
            }])
        }
    }

    impl syscalls::RemoteGit for FixedGit {
        fn commit_for_remote(&self, _url: &str, _path: Option<&Path>) -> Result<String, CommitForRemoteError> {
            Ok("deadbeef".to_string())
        }
    }

    fn test_project_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project")
    }

    /// A writable copy of `test_project`, so a `Save` test can actually
    /// write to disk without touching the repository's own fixture. Named
    /// per test/process/line, same convention `disk`'s own tests use for
    /// scratch directories (see `crates/disk/src/project/operations/
    /// load.rs`'s tests) — caller is responsible for `remove_dir_all`.
    fn scratch_copy_of_test_project(label: &str) -> PathBuf {
        let dest = std::env::temp_dir().join(format!("gui-core-actor-test-{label}-{}", std::process::id()));
        std::fs::remove_dir_all(&dest).ok();
        let status = std::process::Command::new("cp")
            .args(["-r", test_project_dir().to_str().unwrap(), dest.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success(), "failed to copy test_project to {dest:?}");
        dest
    }

    fn entry_name(name: &str) -> EntryName {
        EntryName(name.to_string())
    }

    fn spawn_test_actor() -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>) {
        spawn_test_actor_with_git(FixedGit)
    }

    /// Same as `spawn_test_actor`, but against a caller-supplied `Git`/
    /// `RemoteGit` instead of the always-`"deadbeef"` `FixedGit` — for
    /// tests that need to observe *what* got asked of `git`, not just that
    /// something did (e.g. `entry_directory`'s own path-building, see
    /// `PathEchoingGit` below).
    fn spawn_test_actor_with_git<G>(git: G) -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>)
    where
        G: syscalls::Git + syscalls::RemoteGit + Clone + Send + Sync + 'static,
    {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_actor(command_rx, event_tx, StdFilesystem, git));
        (command_tx, event_rx)
    }

    /// Echoes the path/URL it was asked to resolve a commit for back as
    /// the "commit" itself, instead of returning a fixed value like
    /// `FixedGit` does — lets a test assert on the *exact* filesystem path
    /// `entry_directory` built for a `Command::ResolveLocalCommit`, not
    /// just that resolution succeeded.
    #[derive(Debug, Clone, Copy, Default)]
    struct PathEchoingGit;

    impl syscalls::Git for PathEchoingGit {
        fn commit_for_path_excluding(&self, path: &Path, _excludes: &[&Path]) -> Result<String, CommitForPathError> {
            Ok(path.display().to_string())
        }

        fn changed_paths(&self, dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
            Ok(vec![dir.to_path_buf()])
        }

        fn commit_all(&self, _dir: &Path, _message: &str) -> Result<(), CommitAllError> {
            Ok(())
        }

        fn diff(&self, dir: &Path, path: &Path) -> Result<String, DiffError> {
            Ok(format!("{}:{}", dir.display(), path.display()))
        }

        fn files_changed_in_commit(
            &self,
            dir: &Path,
            _commit: &str,
            _excludes: &[&Path],
        ) -> Result<Vec<PathBuf>, CommitForPathError> {
            Ok(vec![dir.to_path_buf()])
        }

        fn diff_for_commit(&self, dir: &Path, commit: &str, path: &Path) -> Result<String, DiffError> {
            Ok(format!("{}:{commit}:{}", dir.display(), path.display()))
        }
    }

    impl syscalls::RemoteGit for PathEchoingGit {
        fn commit_for_remote(&self, url: &str, path: Option<&Path>) -> Result<String, CommitForRemoteError> {
            Ok(format!("{url}|{}", path.map(|p| p.display().to_string()).unwrap_or_default()))
        }
    }

    /// A controllable block point used by the mutation-exclusion/concurrent-
    /// reads tests below, in place of `FixedGit`'s instant fixed replies —
    /// see README's Testing strategy: proving exclusion actually holds
    /// during execution needs "a deliberately slow/never-resolving fake",
    /// not just fakes that happen to finish in send order (which is all
    /// `commands_sent_during_a_mutation_queue_and_drain_in_order` above can
    /// prove — see its own doc comment). `wait()` blocks the calling
    /// blocking-pool thread on a real `std::sync::mpsc` channel — not a
    /// tokio primitive — since it's called from inside `spawn_blocking`,
    /// off the async runtime entirely.
    #[derive(Clone)]
    struct Gate {
        started: Arc<AtomicBool>,
        release: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    }

    impl Gate {
        fn new() -> (Self, std::sync::mpsc::Sender<()>) {
            let (tx, rx) = std::sync::mpsc::channel();
            (
                Gate {
                    started: Arc::new(AtomicBool::new(false)),
                    release: Arc::new(Mutex::new(Some(rx))),
                },
                tx,
            )
        }

        /// Blocks until the test's sender fires (or is dropped). Panics if
        /// called more than once — each test's `Gate` is single-use, built
        /// fresh per scenario.
        fn wait(&self) {
            self.started.store(true, Ordering::SeqCst);
            let rx = self
                .release
                .lock()
                .unwrap()
                .take()
                .expect("Gate::wait called more than once");
            rx.recv().ok();
        }
    }

    /// Blocks on `Gate::wait` inside `commit_for_remote` — everything else
    /// behaves like `FixedGit`. Used to hold a `Validate` open long enough
    /// for a test to observe what else does (or doesn't) run while it's in
    /// flight.
    #[derive(Clone)]
    struct HangingRemoteGit {
        gate: Gate,
    }

    impl syscalls::Git for HangingRemoteGit {
        fn commit_for_path_excluding(&self, _path: &Path, _excludes: &[&Path]) -> Result<String, CommitForPathError> {
            Ok("deadbeef".to_string())
        }

        fn changed_paths(&self, _dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
            Ok(Vec::new())
        }

        fn commit_all(&self, _dir: &Path, _message: &str) -> Result<(), CommitAllError> {
            Ok(())
        }

        fn diff(&self, _dir: &Path, path: &Path) -> Result<String, DiffError> {
            Ok(format!("diff for {}", path.display()))
        }
    }

    impl syscalls::RemoteGit for HangingRemoteGit {
        fn commit_for_remote(&self, _url: &str, _path: Option<&Path>) -> Result<String, CommitForRemoteError> {
            self.gate.wait();
            Ok("deadbeef".to_string())
        }
    }

    /// Blocks on `Gate::wait` inside `changed_paths` — everything else
    /// behaves like `FixedGit`. Used to hold a read-only, git-backed
    /// command (`GetChangedFiles`) open to prove a second read doesn't
    /// queue behind it.
    #[derive(Clone)]
    struct HangingChangedPathsGit {
        gate: Gate,
    }

    impl syscalls::Git for HangingChangedPathsGit {
        fn commit_for_path_excluding(&self, _path: &Path, _excludes: &[&Path]) -> Result<String, CommitForPathError> {
            Ok("deadbeef".to_string())
        }

        fn changed_paths(&self, _dir: &Path) -> Result<Vec<PathBuf>, ChangedPathsError> {
            self.gate.wait();
            Ok(Vec::new())
        }

        fn commit_all(&self, _dir: &Path, _message: &str) -> Result<(), CommitAllError> {
            Ok(())
        }

        fn diff(&self, _dir: &Path, path: &Path) -> Result<String, DiffError> {
            Ok(format!("diff for {}", path.display()))
        }
    }

    impl syscalls::RemoteGit for HangingChangedPathsGit {
        fn commit_for_remote(&self, _url: &str, _path: Option<&Path>) -> Result<String, CommitForRemoteError> {
            Ok("deadbeef".to_string())
        }
    }

    /// A remote dependency `Validate` must resolve — attached to a fresh
    /// requirement so any `Git`/`RemoteGit` fake's `commit_for_remote` is
    /// guaranteed to run during validation.
    fn add_requirement_with_remote_dependency_command(name: &str, request: RequestId) -> Command {
        let mut requirement = RequirementDraft::new("Needs Remote");
        requirement.requirement_text = "Text".to_string();
        requirement
            .dependencies
            .push(DependencyReferenceKind::RemoteReferenceV1(RemoteGitReference {
                url: "https://example.com/repo.git".to_string(),
                path: None,
                commit: "deadbeef".to_string(),
            }));
        Command::AddRequirement {
            module: vec![],
            name: entry_name(name),
            requirement: Box::new(requirement),
            request,
        }
    }

    /// Drains `events` until the `Event::Completed` for `request` shows up,
    /// discarding any `TreeChanged`/other requests' completions in between
    /// — mirrors how `gui-ui` matches a reply by `RequestId`, not by
    /// assuming events arrive in lockstep with one outstanding request.
    async fn recv_completed(events: &mut mpsc::UnboundedReceiver<Event>, request: RequestId) -> Outcome {
        loop {
            match events.recv().await.expect("actor task ended without completing") {
                Event::Completed { request: r, outcome } if r == request => return outcome,
                _ => continue,
            }
        }
    }

    /// Drains `events` until the next `Event::TreeChanged` shows up,
    /// discarding any `Completed` events in between — for tests checking
    /// `can_undo`/`can_redo`, which only `TreeChanged` carries.
    async fn recv_tree_changed(events: &mut mpsc::UnboundedReceiver<Event>) -> TreeSnapshot {
        loop {
            match events.recv().await.expect("actor task ended without a TreeChanged") {
                Event::TreeChanged(snapshot) => return snapshot,
                _ => continue,
            }
        }
    }

    /// Waits out a fixed window asserting none of `requests` completes in
    /// it — used by the `Gate`-based tests to prove something stayed
    /// blocked, not just "hadn't been polled yet." Ignores unrelated events
    /// (e.g. a stray `TreeChanged` left over from an earlier command) rather
    /// than treating any activity on the channel as a completion.
    async fn assert_none_complete_within(
        events: &mut mpsc::UnboundedReceiver<Event>,
        requests: &[RequestId],
        duration: std::time::Duration,
    ) {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(Event::Completed { request, .. })) if requests.contains(&request) => {
                    panic!("request {request} completed before it should have");
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("actor task ended without completing"),
                Err(_) => return,
            }
        }
    }

    fn add_requirement_command(module: Vec<EntryName>, name: &str, title: &str, request: RequestId) -> Command {
        let mut requirement = RequirementDraft::new(title);
        requirement.requirement_text = "Text".to_string();
        Command::AddRequirement {
            module,
            name: entry_name(name),
            requirement: Box::new(requirement),
            request,
        }
    }

    #[tokio::test]
    async fn add_requirement_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(add_requirement_command(vec![], "design", "Design", 1))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn get_entry_detail_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn load_project_then_add_requirement_then_read_it_back() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("scratch"))),
                request: 3,
            })
            .unwrap();
        match recv_completed(&mut events, 3).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { title, met_status, .. })) => {
                assert_eq!(title, "Scratch");
                // The project was loaded but never `Validate`d in this
                // test — nothing resolved to check Met/Unmet against.
                assert!(matches!(met_status, RequirementMetStatus::Unvalidated));
            }
            other => panic!("expected EntryDetail(Some(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_entry_detail_after_validate_reports_the_real_met_status() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Validate { request: 2 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::Validate(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                request: 3,
            })
            .unwrap();
        match recv_completed(&mut events, 3).await {
            // "design" is a real `test_project` requirement whose
            // own test reference is stale against this repo's real git
            // history (see `crates/gui-ui/tests/interaction.rs`'s
            // `a_validated_requirements_tree_leaf_shows_the_unmet_status_icon`
            // for the same fact, established there empirically) — Unmet
            // with a real, non-placeholder reason.
            Outcome::EntryDetail(Some(EntryDetail::Requirement { met_status, .. })) => {
                assert!(matches!(met_status, RequirementMetStatus::Unmet(UnmetReason::UnsatisfiedTests(_))));
            }
            other => panic!("expected EntryDetail(Some(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_requirement_met_status_reports_unvalidated_then_the_real_status_after_validate() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // Not validated yet in this session — same "nothing resolved to
        // check" reasoning `get_entry_detail`'s own Draft-state test uses.
        commands
            .send(Command::GetRequirementMetStatus {
                target: LogicalPath::root(entry_name("design")),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::RequirementMetStatus(RequirementMetStatus::Unvalidated)
        ));

        commands.send(Command::Validate { request: 3 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Validate(Ok(()))));

        commands
            .send(Command::GetRequirementMetStatus {
                target: LogicalPath::root(entry_name("design")),
                request: 4,
            })
            .unwrap();
        // Same real fact `get_entry_detail_after_validate_reports_the_real_met_status`
        // establishes: "design"'s own test reference is genuinely
        // stale against this repo's real git history.
        assert!(matches!(
            recv_completed(&mut events, 4).await,
            Outcome::RequirementMetStatus(RequirementMetStatus::Unmet(UnmetReason::UnsatisfiedTests(_)))
        ));
    }

    #[tokio::test]
    async fn load_project_then_add_requirement_with_a_dependency_then_read_it_back() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut requirement = RequirementDraft::new("Scratch");
        requirement.requirement_text = "Text".to_string();
        requirement.dependencies.push(disk::DependencyReferenceKind::RequirementReferenceV1(
            disk::LocalGitReference {
                path: disk::ReferencePath("/requirements/design".to_string()),
                commit: "abc123".to_string(),
            },
        ));
        commands
            .send(Command::AddRequirement {
                module: vec![],
                name: entry_name("scratch"),
                requirement: Box::new(requirement),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("scratch"))),
                request: 3,
            })
            .unwrap();
        match recv_completed(&mut events, 3).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { dependencies, .. })) => {
                assert_eq!(dependencies.len(), 1);
                assert!(matches!(
                    &dependencies[0],
                    disk::DependencyReferenceKind::RequirementReferenceV1(local)
                        if local.path.0 == "/requirements/design" && local.commit == "abc123"
                ));
            }
            other => panic!("expected EntryDetail(Some(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_requirement_into_a_missing_module_reports_module_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![entry_name("does_not_exist")], "x", "X", 2))
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddRequirement(Err(AddChildError::ModuleNotFound))
        ));
    }

    #[tokio::test]
    async fn add_then_remove_requirement_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::RemoveRequirement {
                target: LogicalPath::root(entry_name("scratch")),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RemoveRequirement(true)
        ));

        commands
            .send(Command::RemoveRequirement {
                target: LogicalPath::root(entry_name("scratch")),
                request: 4,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 4).await,
            Outcome::RemoveRequirement(false)
        ));
    }

    /// Exercises README's "Exactly one mutation at a time": fires a
    /// spawned mutation (`Validate`) and, without waiting for it, fires a
    /// mutating and a read-only command right behind it. All three must
    /// still complete correctly once `Validate` finishes and the queue
    /// drains — proving `dispatch`'s exclusion check and `drain_queue`
    /// don't drop or reorder queued work. (This doesn't prove the queued
    /// commands couldn't have run *during* `Validate` without a
    /// controllable fake to hold it open — seeREADME's "Known gap" — it
    /// proves the queue/drain mechanism itself is correct, which is what
    /// actually enforces the exclusion regardless of how long any given
    /// mutation takes.)
    #[tokio::test]
    async fn commands_sent_during_a_mutation_queue_and_drain_in_order() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Validate { request: 2 }).unwrap();
        commands
            .send(add_requirement_command(vec![], "after_validate", "After Validate", 3))
            .unwrap();
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("after_validate"))),
                request: 4,
            })
            .unwrap();

        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::Validate(Ok(()))));
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::AddRequirement(Ok(()))));
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { title, .. })) => {
                assert_eq!(title, "After Validate")
            }
            other => panic!("expected EntryDetail(Some(_)), got {other:?}"),
        }
    }

    /// Closes the gap the test above's own doc comment names: with
    /// `HangingRemoteGit` deliberately holding `Validate` open, a second
    /// mutating command sent while it's in flight must not even start —
    /// not just "happens to finish after" — until `Validate` completes.
    #[tokio::test]
    async fn a_mutation_is_excluded_while_another_is_in_flight() {
        let (git, release) = Gate::new();
        let (commands, mut events) = spawn_test_actor_with_git(HangingRemoteGit { gate: git });

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_with_remote_dependency_command("needs_remote", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands.send(Command::Validate { request: 3 }).unwrap();
        commands
            .send(add_requirement_command(vec![], "queued", "Queued", 4))
            .unwrap();

        // `Validate` is genuinely blocked inside `commit_for_remote` right
        // now (not just "hasn't been polled yet") — so this window isn't
        // racing a real duration, it's asserting against a hold that only
        // this test's `release` can end.
        assert_none_complete_within(&mut events, &[3, 4], std::time::Duration::from_millis(200)).await;

        release.send(()).unwrap();

        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Validate(Ok(()))));
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::AddRequirement(Ok(()))));
    }

    /// The flip side of the test above, and the crate's own documented
    /// "Known gap": `dispatch`'s exclusion check queues *every* command
    /// while a mutation is in flight, including read-only ones — even
    /// though `spawn_read`'s own doc comment says reads never race the
    /// actor and could in principle run concurrently with anything. This
    /// pins that current behavior down as a regression test, not a fix
    /// (the README treats fixing it as `syscalls`/`logical`-layer work).
    #[tokio::test]
    async fn a_read_queues_behind_an_in_flight_mutation() {
        let (git, release) = Gate::new();
        let (commands, mut events) = spawn_test_actor_with_git(HangingRemoteGit { gate: git });

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_with_remote_dependency_command("needs_remote", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands.send(Command::Validate { request: 3 }).unwrap();
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("needs_remote"))),
                request: 4,
            })
            .unwrap();

        assert_none_complete_within(&mut events, &[3, 4], std::time::Duration::from_millis(200)).await;

        release.send(()).unwrap();

        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Validate(Ok(()))));
        assert!(matches!(
            recv_completed(&mut events, 4).await,
            Outcome::EntryDetail(Some(EntryDetail::Requirement { .. }))
        ));
    }

    /// Unlike a command queued behind an in-flight mutation (the two tests
    /// above), two reads sent with nothing in flight aren't serialized
    /// against each other — `spawn_read` gives each its own task, so a
    /// slower one doesn't hold up a faster one behind it. Proven with
    /// `HangingChangedPathsGit` rather than send order, since send order
    /// alone can't distinguish "ran concurrently" from "happened to finish
    /// fast."
    #[tokio::test]
    async fn independent_reads_are_not_serialized_against_each_other() {
        let (git, release) = Gate::new();
        let (commands, mut events) = spawn_test_actor_with_git(HangingChangedPathsGit { gate: git });

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // Slow (blocked on `release`), sent first.
        commands.send(Command::GetChangedFiles { request: 2 }).unwrap();
        // Fast (pure in-memory), sent second — must not wait for request 2.
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                request: 3,
            })
            .unwrap();

        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::EntryDetail(Some(EntryDetail::Requirement { .. }))
        ));

        release.send(()).unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::GetChangedFiles(Ok(_))));
    }

    /// `logical::validate::validate` drops the draft on `Err` (see the
    /// comment in `spawn_validate`) — this confirms the clone-before-
    /// validate restore actually works end to end: the broken requirement
    /// is still there, still editable, after a failed `Validate`.
    #[tokio::test]
    async fn a_failed_validate_restores_an_editable_draft() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut broken = RequirementDraft::new("Broken");
        broken.requirement_text = "Text".to_string();
        broken.tests.push(TestReferenceKind::TestReferenceV1(LocalGitReference {
            path: ReferencePath("/tests/does_not_exist".to_string()),
            commit: "deadbeef".to_string(),
        }));
        commands
            .send(Command::AddRequirement {
                module: vec![],
                name: entry_name("broken"),
                requirement: Box::new(broken),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands.send(Command::Validate { request: 3 }).unwrap();
        match recv_completed(&mut events, 3).await {
            Outcome::Validate(Err(errors)) => assert!(!errors.is_empty()),
            other => panic!("expected Validate(Err(_)), got {other:?}"),
        }

        // Still there and still editable — proves the draft was restored,
        // not lost, and the project wasn't left stuck as `Validated`.
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("broken"))),
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { title, .. })) => {
                assert_eq!(title, "Broken")
            }
            other => panic!("expected EntryDetail(Some(_)), got {other:?}"),
        }
        commands
            .send(Command::RemoveRequirement {
                target: LogicalPath::root(entry_name("broken")),
                request: 5,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 5).await,
            Outcome::RemoveRequirement(true)
        ));
    }

    /// Editing a `Validated` project auto-demotes it to `Draft` first
    /// (rather than erroring) — see README's "Project state: alternating
    /// Draft/Validated, one owner".
    #[tokio::test]
    async fn editing_a_validated_project_demotes_it_to_draft() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Validate { request: 2 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::Validate(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 3))
            .unwrap();
        // Succeeding at all (rather than e.g. panicking on an
        // unreachable!() in `ensure_draft`) is the behavior under test —
        // `add_requirement` only works on a `Draft`, so this only passes
        // if the Validated -> Draft demotion actually ran first.
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::AddRequirement(Ok(()))));
    }

    #[tokio::test]
    async fn save_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();
        commands.send(Command::Save { request: 1 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn save_without_validating_first_reports_not_validated() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Save { request: 2 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::Save(Err(SaveError::NotValidated))
        ));
    }

    #[tokio::test]
    async fn validate_then_save_writes_the_new_requirement_to_disk() {
        let dir = scratch_copy_of_test_project("save-writes-to-disk");
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: dir.clone(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands.send(Command::Validate { request: 3 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Validate(Ok(()))));

        commands.send(Command::Save { request: 4 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::Save(Ok(()))));

        assert!(dir.join("requirements/scratch/requirement.ron").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn new_project_replaces_whatever_was_loaded() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::NewProject {
                name: "Scratch Project".to_string(),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::NewProject));

        // The old project's "design" requirement is gone — this
        // really replaced the state, not merged into it.
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::EntryDetail(None)));

        // No path yet — `NewProject` doesn't inherit the previous
        // project's path, and a plain `Save` needs one already set (see
        // `Command::SaveAs`'s doc comment).
        commands.send(Command::Save { request: 4 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn undo_without_any_mutation_reports_nothing_to_undo() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Undo { request: 2 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::Undo(Err(UndoError::NothingToUndo))
        ));
    }

    #[tokio::test]
    async fn redo_without_any_undo_reports_nothing_to_redo() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Redo { request: 2 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::Redo(Err(RedoError::NothingToRedo))
        ));
    }

    #[tokio::test]
    async fn undo_reverts_the_last_add_requirement_and_redo_reapplies_it() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("scratch"))),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::EntryDetail(Some(_))));

        commands.send(Command::Undo { request: 4 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::Undo(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("scratch"))),
                request: 5,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 5).await, Outcome::EntryDetail(None)));

        commands.send(Command::Redo { request: 6 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 6).await, Outcome::Redo(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("scratch"))),
                request: 7,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 7).await, Outcome::EntryDetail(Some(_))));
    }

    #[tokio::test]
    async fn a_new_mutation_after_undo_clears_redo_history() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "first", "First", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands.send(Command::Undo { request: 3 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Undo(Ok(()))));

        // A different edit, not a redo — this should invalidate the redo
        // history the `Undo` above just created, the usual "a fresh edit
        // invalidates redo" rule.
        commands
            .send(add_requirement_command(vec![], "second", "Second", 4))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::AddRequirement(Ok(()))));

        commands.send(Command::Redo { request: 5 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 5).await,
            Outcome::Redo(Err(RedoError::NothingToRedo))
        ));
    }

    #[tokio::test]
    async fn new_project_clears_undo_history() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::NewProject {
                name: "Scratch Project".to_string(),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::NewProject));

        commands.send(Command::Undo { request: 4 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 4).await,
            Outcome::Undo(Err(UndoError::NothingToUndo))
        ));
    }

    #[tokio::test]
    async fn a_successful_load_project_clears_undo_history() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        // Reload the same project — a fresh `LoadProject`, not a
        // continuation of the in-memory edit above.
        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Undo { request: 4 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 4).await,
            Outcome::Undo(Err(UndoError::NothingToUndo))
        ));
    }

    #[tokio::test]
    async fn tree_changed_reports_can_undo_and_can_redo() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));
        let snapshot = recv_tree_changed(&mut events).await;
        assert!(!snapshot.can_undo);
        assert!(!snapshot.can_redo);

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));
        let snapshot = recv_tree_changed(&mut events).await;
        assert!(snapshot.can_undo);
        assert!(!snapshot.can_redo);

        commands.send(Command::Undo { request: 3 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Undo(Ok(()))));
        let snapshot = recv_tree_changed(&mut events).await;
        assert!(!snapshot.can_undo);
        assert!(snapshot.can_redo);
    }

    #[tokio::test]
    async fn save_as_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::SaveAs {
                path: PathBuf::from("/nonexistent"),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn save_as_an_unvalidated_project_reports_not_validated() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::NewProject {
                name: "Scratch".to_string(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NewProject));

        commands
            .send(Command::SaveAs {
                path: PathBuf::from("/nonexistent"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::SaveAs(Err(SaveError::NotValidated))
        ));
    }

    #[tokio::test]
    async fn new_project_then_save_as_writes_a_fresh_project_to_a_new_directory() {
        let dir = std::env::temp_dir().join(format!("gui-core-actor-test-new-project-save-as-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::NewProject {
                name: "Scratch".to_string(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NewProject));

        commands
            .send(add_requirement_command(vec![], "scratch", "Scratch", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands.send(Command::Validate { request: 3 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Validate(Ok(()))));

        commands
            .send(Command::SaveAs {
                path: dir.clone(),
                request: 4,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::SaveAs(Ok(()))));

        assert!(dir.join("requirements/scratch/requirement.ron").exists());

        // The just-picked path is now the project's home — a plain
        // `Save` (no path of its own) works against it without needing
        // another `SaveAs`.
        commands
            .send(add_requirement_command(vec![], "second", "Second", 5))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 5).await, Outcome::AddRequirement(Ok(()))));
        commands.send(Command::Validate { request: 6 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 6).await, Outcome::Validate(Ok(()))));
        commands.send(Command::Save { request: 7 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 7).await, Outcome::Save(Ok(()))));
        assert!(dir.join("requirements/second/requirement.ron").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_save_as_does_not_change_the_known_project_path() {
        let dir = scratch_copy_of_test_project("save-as-failure-keeps-path");
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: dir.clone(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));
        commands.send(Command::Validate { request: 2 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::Validate(Ok(()))));

        // A file (not a directory) as the target — `save()` can't create
        // `requirements/` etc. underneath it, so this fails partway
        // through rather than up front; which specific error it is isn't
        // this test's concern, only that the *known* project path
        // doesn't change because of it.
        let blocked = dir.join("blocked-target");
        std::fs::write(&blocked, b"not a directory").unwrap();
        commands
            .send(Command::SaveAs {
                path: blocked.clone(),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::SaveAs(Err(_))));

        // Still targets `dir`, not `blocked` — an ordinary `Save` (which
        // only ever uses the *known* path) still writes there successfully.
        commands.send(Command::Save { request: 4 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::Save(Ok(()))));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn add_then_remove_module_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddModule {
                module: vec![],
                name: entry_name("scratch_module"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddModule(Ok(()))));

        // A requirement inside the new submodule proves `AddRequirement`'s
        // `module` addressing (parent chain + name) works for a nested
        // module too, not just the project root.
        commands
            .send(add_requirement_command(
                vec![entry_name("scratch_module")],
                "nested",
                "Nested",
                3,
            ))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::RemoveModule {
                target: vec![entry_name("scratch_module")],
                request: 4,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::RemoveModule(true)));

        commands
            .send(Command::RemoveModule {
                target: vec![entry_name("scratch_module")],
                request: 5,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 5).await, Outcome::RemoveModule(false)));
    }

    #[tokio::test]
    async fn remove_module_on_the_project_root_reports_false_instead_of_panicking() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::RemoveModule {
                target: vec![], // the project root itself
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::RemoveModule(false)));
    }

    #[tokio::test]
    async fn add_then_remove_test_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut scratch_test = logical::draft::TestDraft::new("Scratch Test", disk::ResultKindV1::FreeForm);
        scratch_test.test_text = "Text".to_string();
        commands
            .send(Command::AddTest {
                module: vec![],
                name: entry_name("scratch_test"),
                test: Box::new(scratch_test),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddTest(Ok(()))));

        commands
            .send(Command::RemoveTest {
                target: LogicalPath::root(entry_name("scratch_test")),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::RemoveTest(true)));
    }

    #[tokio::test]
    async fn add_then_remove_result_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let result = logical::draft::ResultDraft::new(
            "Scratch Result",
            "deadbeef",
            ReferencePath("/tests/generic_test".to_string()),
            "deadbeef",
        );
        commands
            .send(Command::AddResult {
                requirement: LogicalPath::root(entry_name("design")),
                name: entry_name("scratch_result"),
                result: Box::new(result),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddResult(Ok(()))));

        commands
            .send(Command::RemoveResult {
                target: logical::ResultPath {
                    requirement: LogicalPath::root(entry_name("design")),
                    name: entry_name("scratch_result"),
                },
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::RemoveResult(true)));
    }

    #[tokio::test]
    async fn add_test_into_a_missing_module_reports_module_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddTest {
                module: vec![entry_name("does_not_exist")],
                name: entry_name("x"),
                test: Box::new(logical::draft::TestDraft::new("X", disk::ResultKindV1::FreeForm)),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddTest(Err(AddChildError::ModuleNotFound))
        ));
    }

    #[tokio::test]
    async fn update_requirement_replaces_content_and_read_reflects_it() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Original", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        let mut updated_requirement = RequirementDraft::new("Updated");
        updated_requirement.requirement_text = "Updated text".to_string();
        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath::root(entry_name("scratch")),
                requirement: Box::new(updated_requirement),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::UpdateRequirement(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("scratch"))),
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { title, .. })) => {
                assert_eq!(title, "Updated")
            }
            other => panic!("expected EntryDetail(Some(Requirement)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_requirement_on_a_nonexistent_entry_reports_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut updated_requirement = RequirementDraft::new("Updated");
        updated_requirement.requirement_text = "Updated text".to_string();
        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath::root(entry_name("does_not_exist")),
                requirement: Box::new(updated_requirement),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::UpdateRequirement(Err(UpdateChildError::NotFound))
        ));
    }

    #[tokio::test]
    async fn update_requirement_into_a_missing_module_reports_module_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath {
                    modules: vec![entry_name("does_not_exist")],
                    name: entry_name("whatever"),
                },
                requirement: Box::new(RequirementDraft::new("Updated")),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::UpdateRequirement(Err(UpdateChildError::ModuleNotFound))
        ));
    }

    #[tokio::test]
    async fn refresh_stale_test_references_without_validating_first_reports_not_validated() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // Not validated in this session — no "current" test commits known
        // to correct anything to.
        commands
            .send(Command::RefreshStaleTestReferences {
                target: LogicalPath::root(entry_name("design")),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::RefreshStaleTestReferences(Err(RefreshStaleTestReferencesError::NotValidated))
        ));
    }

    #[tokio::test]
    async fn refresh_stale_test_references_on_a_nonexistent_entry_reports_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Validate { request: 2 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::Validate(Ok(()))));

        commands
            .send(Command::RefreshStaleTestReferences {
                target: LogicalPath::root(entry_name("does_not_exist")),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RefreshStaleTestReferences(Err(RefreshStaleTestReferencesError::Update(
                UpdateChildError::NotFound
            )))
        ));
    }

    #[tokio::test]
    async fn refresh_stale_test_references_fixes_the_stale_reference() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Validate { request: 2 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::Validate(Ok(()))));

        // "design" is genuinely `Unmet` with a `StaleReference` before
        // the fix — same real fact
        // `get_entry_detail_after_validate_reports_the_real_met_status`
        // establishes.
        let target = LogicalPath::root(entry_name("design"));
        commands
            .send(Command::RefreshStaleTestReferences {
                target: target.clone(),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::RefreshStaleTestReferences(Ok(()))));

        // The fix is itself an edit, so it demotes back to `Draft` same
        // as any other — but `RefreshStaleTestReferences` implicitly
        // revalidates on success, so by the time it completes the project
        // is already re-`Validated` and this reads the real status
        // straight away, with no separate `Validate` call needed. The
        // reference itself is current now, so the remaining reason (if
        // any) can no longer be `StaleReference` — `test_project`'s
        // results are all `Incomplete`, not `Pass`, so it's still
        // `Unmet`, just for a different, real reason (`NoPassingResult`)
        // than before the fix.
        commands
            .send(Command::GetRequirementMetStatus { target, request: 4 })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::RequirementMetStatus(RequirementMetStatus::Unmet(UnmetReason::UnsatisfiedTests(tests))) => {
                assert_eq!(tests.len(), 1);
                assert!(matches!(tests[0].reason, TestUnmetReason::NoPassingResult));
            }
            other => panic!("expected Unmet(UnsatisfiedTests(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_stale_result_reference_without_validating_first_reports_not_validated() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // Not validated in this session — no "current" requirement/test
        // commits known to correct anything to.
        commands
            .send(Command::RefreshStaleResultReference {
                target: logical::ResultPath {
                    requirement: LogicalPath::root(entry_name("design")),
                    name: entry_name("design"),
                },
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::RefreshStaleResultReference(Err(RefreshStaleTestReferencesError::NotValidated))
        ));
    }

    #[tokio::test]
    async fn refresh_stale_result_reference_on_a_nonexistent_entry_reports_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Validate { request: 2 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::Validate(Ok(()))));

        commands
            .send(Command::RefreshStaleResultReference {
                target: logical::ResultPath {
                    requirement: LogicalPath::root(entry_name("design")),
                    name: entry_name("does_not_exist"),
                },
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RefreshStaleResultReference(Err(RefreshStaleTestReferencesError::Update(
                UpdateChildError::NotFound
            )))
        ));
    }

    #[tokio::test]
    async fn refresh_stale_result_reference_fixes_the_stale_reference() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // A fresh result pinned to obviously wrong commits — deliberately
        // not this repo's own real current commit, so the test doesn't
        // depend on that hash, only on it being *different*.
        // `FixedGit` always answers "deadbeef" for any path's *current*
        // commit (see its own doc comment), so a genuinely stale pin here
        // has to be anything else.
        let stale_result = logical::draft::ResultDraft::new(
            "Stale",
            "obsolete-requirement-commit",
            ReferencePath("/tests/smoke".to_string()),
            "obsolete-test-commit",
        );
        commands
            .send(Command::AddResult {
                requirement: LogicalPath::root(entry_name("design")),
                name: entry_name("scratch_result"),
                result: Box::new(stale_result),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddResult(Ok(()))));

        commands.send(Command::Validate { request: 3 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::Validate(Ok(()))));

        let result_path = logical::ResultPath {
            requirement: LogicalPath::root(entry_name("design")),
            name: entry_name("scratch_result"),
        };
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Result(result_path.clone()),
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Result {
                stale,
                requirement_commit,
                test_commit,
                ..
            })) => {
                assert!(stale);
                assert_eq!(requirement_commit, "obsolete-requirement-commit");
                assert_eq!(test_commit, "obsolete-test-commit");
            }
            other => panic!("expected EntryDetail(Some(Result)), got {other:?}"),
        }

        commands
            .send(Command::RefreshStaleResultReference {
                target: result_path.clone(),
                request: 5,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 5).await,
            Outcome::RefreshStaleResultReference(Ok(()))
        ));

        // Implicitly revalidated on success — see `refresh_stale_test_
        // references_fixes_the_stale_reference`'s own comment on why no
        // separate `Validate` call is needed before this re-fetch.
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Result(result_path),
                request: 6,
            })
            .unwrap();
        match recv_completed(&mut events, 6).await {
            Outcome::EntryDetail(Some(EntryDetail::Result {
                stale,
                requirement_commit,
                test_commit,
                ..
            })) => {
                assert!(!stale);
                assert_eq!(requirement_commit, "deadbeef");
                assert_eq!(test_commit, "deadbeef");
            }
            other => panic!("expected EntryDetail(Some(Result)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_test_replaces_content() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut original_test = logical::draft::TestDraft::new("Original", disk::ResultKindV1::FreeForm);
        original_test.test_text = "Text".to_string();
        commands
            .send(Command::AddTest {
                module: vec![],
                name: entry_name("scratch_test"),
                test: Box::new(original_test),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddTest(Ok(()))));

        let mut updated_test = logical::draft::TestDraft::new("Updated", disk::ResultKindV1::Template);
        updated_test.test_text = "Updated text".to_string();
        commands
            .send(Command::UpdateTest {
                target: LogicalPath::root(entry_name("scratch_test")),
                test: Box::new(updated_test),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::UpdateTest(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Test(LogicalPath::root(entry_name("scratch_test"))),
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Test { title, result_kind, .. })) => {
                assert_eq!(title, "Updated");
                assert!(matches!(result_kind, disk::ResultKindV1::Template));
            }
            other => panic!("expected EntryDetail(Some(Test)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_result_replaces_content() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let original = logical::draft::ResultDraft::new(
            "Original",
            "deadbeef",
            ReferencePath("/tests/generic_test".to_string()),
            "deadbeef",
        );
        commands
            .send(Command::AddResult {
                requirement: LogicalPath::root(entry_name("design")),
                name: entry_name("scratch_result"),
                result: Box::new(original),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddResult(Ok(()))));

        let updated = logical::draft::ResultDraft::new(
            "Updated",
            "cafef00d",
            ReferencePath("/tests/generic_test".to_string()),
            "cafef00d",
        );
        let result_path = logical::ResultPath {
            requirement: LogicalPath::root(entry_name("design")),
            name: entry_name("scratch_result"),
        };
        commands
            .send(Command::UpdateResult {
                target: result_path.clone(),
                result: Box::new(updated),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::UpdateResult(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Result(result_path),
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Result { title, requirement, .. })) => {
                assert_eq!(title, "Updated");
                assert_eq!(requirement, LogicalPath::root(entry_name("design")));
            }
            other => panic!("expected EntryDetail(Some(Result)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_result_with_a_blank_title_autopopulates_it_from_the_name() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let original = logical::draft::ResultDraft::new(
            "Original",
            "deadbeef",
            ReferencePath("/tests/generic_test".to_string()),
            "deadbeef",
        );
        commands
            .send(Command::AddResult {
                requirement: LogicalPath::root(entry_name("design")),
                name: entry_name("scratch_result"),
                result: Box::new(original),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddResult(Ok(()))));

        let updated = logical::draft::ResultDraft::new(
            "",
            "cafef00d",
            ReferencePath("/tests/generic_test".to_string()),
            "cafef00d",
        );
        let result_path = logical::ResultPath {
            requirement: LogicalPath::root(entry_name("design")),
            name: entry_name("scratch_result"),
        };
        commands
            .send(Command::UpdateResult {
                target: result_path.clone(),
                result: Box::new(updated),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::UpdateResult(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Result(result_path),
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Result { title, .. })) => {
                assert_eq!(title, "Scratch Result");
            }
            other => panic!("expected EntryDetail(Some(Result)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_then_remove_attachment_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddAttachment {
                module: vec![],
                path: PathBuf::from("scratch_attachment.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddAttachment(Ok(()))));

        commands
            .send(Command::RemoveAttachment {
                module: vec![],
                path: PathBuf::from("scratch_attachment.md"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RemoveAttachment(true)
        ));

        commands
            .send(Command::RemoveAttachment {
                module: vec![],
                path: PathBuf::from("scratch_attachment.md"),
                request: 4,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 4).await,
            Outcome::RemoveAttachment(false)
        ));
    }

    #[tokio::test]
    async fn add_then_remove_template_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddTemplate {
                module: vec![],
                path: PathBuf::from("scratch_template.typ"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddTemplate(Ok(()))));

        commands
            .send(Command::RemoveTemplate {
                module: vec![],
                path: PathBuf::from("scratch_template.typ"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RemoveTemplate(true)
        ));
    }

    #[tokio::test]
    async fn add_attachment_into_a_missing_module_reports_module_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddAttachment {
                module: vec![entry_name("does_not_exist")],
                path: PathBuf::from("x.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddAttachment(Err(AddPoolChildError::ModuleNotFound))
        ));
    }

    /// `test_project`'s root `attachments/overview.md` is already
    /// physically present, so loading the project already populates the
    /// root module's pool with it — adding it again should collide.
    #[tokio::test]
    async fn add_attachment_rejects_a_duplicate_path() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddAttachment {
                module: vec![],
                path: PathBuf::from("overview.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddAttachment(Err(AddPoolChildError::Add(AddPoolFileError::AlreadyExists(_))))
        ));
    }

    #[tokio::test]
    async fn get_module_pools_reflects_an_added_attachment() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddAttachment {
                module: vec![],
                path: PathBuf::from("scratch_attachment.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddAttachment(Ok(()))));

        commands
            .send(Command::GetModulePools {
                module: vec![],
                request: 3,
            })
            .unwrap();
        match recv_completed(&mut events, 3).await {
            Outcome::ModulePools(Some(pools)) => {
                // The root already carries overview.md (physically present
                // in test_project) plus the one just added.
                assert!(pools.attachments.contains(&PathBuf::from("overview.md")));
                assert!(pools.attachments.contains(&PathBuf::from("scratch_attachment.md")));
            }
            other => panic!("expected ModulePools(Some(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_module_pools_for_a_missing_module_reports_none() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetModulePools {
                module: vec![entry_name("does_not_exist")],
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::ModulePools(None)));
    }

    #[tokio::test]
    async fn add_then_remove_a_requirements_local_attachment_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = LogicalPath::root(entry_name("design"));
        commands
            .send(Command::AddRequirementAttachment {
                target: target.clone(),
                path: PathBuf::from("scratch_local.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddRequirementAttachment(Ok(()))
        ));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(target.clone()),
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { attachments, .. })) => {
                assert!(attachments.contains(&PathBuf::from("scratch_local.md")));
            }
            other => panic!("expected EntryDetail(Some(Requirement)), got {other:?}"),
        }

        commands
            .send(Command::RemoveRequirementAttachment {
                target,
                path: PathBuf::from("scratch_local.md"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RemoveRequirementAttachment(true)
        ));
    }

    #[tokio::test]
    async fn add_requirement_attachment_on_a_nonexistent_requirement_reports_entry_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddRequirementAttachment {
                target: LogicalPath::root(entry_name("does_not_exist")),
                path: PathBuf::from("x.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddRequirementAttachment(Err(AddLocalPoolError::EntryNotFound))
        ));
    }

    #[tokio::test]
    async fn add_requirement_attachment_into_a_missing_module_reports_module_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddRequirementAttachment {
                target: LogicalPath {
                    modules: vec![entry_name("does_not_exist")],
                    name: entry_name("whatever"),
                },
                path: PathBuf::from("x.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddRequirementAttachment(Err(AddLocalPoolError::ModuleNotFound))
        ));
    }

    #[tokio::test]
    async fn add_then_remove_a_tests_local_attachment_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = LogicalPath::root(entry_name("smoke"));
        commands
            .send(Command::AddTestAttachment {
                target: target.clone(),
                path: PathBuf::from("scratch_local.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddTestAttachment(Ok(()))));

        commands
            .send(Command::RemoveTestAttachment {
                target,
                path: PathBuf::from("scratch_local.md"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RemoveTestAttachment(true)
        ));
    }

    #[tokio::test]
    async fn add_then_remove_a_tests_local_template_file_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = LogicalPath::root(entry_name("smoke"));
        commands
            .send(Command::AddTestTemplateFile {
                target: target.clone(),
                path: PathBuf::from("scratch_template.typ"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddTestTemplateFile(Ok(()))
        ));

        commands
            .send(Command::RemoveTestTemplateFile {
                target,
                path: PathBuf::from("scratch_template.typ"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RemoveTestTemplateFile(true)
        ));
    }

    #[tokio::test]
    async fn add_then_remove_a_results_local_attachment_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = logical::ResultPath {
            requirement: LogicalPath::root(entry_name("design")),
            name: entry_name("design"),
        };
        commands
            .send(Command::AddResultAttachment {
                target: target.clone(),
                path: PathBuf::from("scratch_local.md"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::AddResultAttachment(Ok(()))
        ));

        commands
            .send(Command::RemoveResultAttachment {
                target,
                path: PathBuf::from("scratch_local.md"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RemoveResultAttachment(true)
        ));
    }

    #[tokio::test]
    async fn rename_module_preserves_its_content() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // "beta" is a real submodule in test_project — add a
        // requirement into it first, to prove the rename doesn't lose
        // the module's existing content.
        commands
            .send(add_requirement_command(vec![entry_name("beta")], "marker", "Marker", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::RenameModule {
                target: vec![entry_name("beta")],
                new_name: entry_name("renamed_setup"),
                reference_actions: Vec::new(),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::RenameModule(Ok(()))));

        // The old name is gone...
        commands
            .send(Command::GetModulePools {
                module: vec![entry_name("beta")],
                request: 4,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::ModulePools(None)));

        // ...and the new name has the module's original content.
        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath {
                    modules: vec![entry_name("renamed_setup")],
                    name: entry_name("marker"),
                }),
                request: 5,
            })
            .unwrap();
        match recv_completed(&mut events, 5).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { title, .. })) => assert_eq!(title, "Marker"),
            other => panic!("expected EntryDetail(Some(Requirement)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rename_module_on_the_project_root_reports_cannot_rename_root() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::RenameModule {
                target: vec![],
                new_name: entry_name("whatever"),
                reference_actions: Vec::new(),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::RenameModule(Err(RenameModuleError::CannotRenameRoot))
        ));
    }

    #[tokio::test]
    async fn rename_module_to_an_existing_name_reports_already_exists() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddModule {
                module: vec![],
                name: entry_name("another_module"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddModule(Ok(()))));

        commands
            .send(Command::RenameModule {
                target: vec![entry_name("beta")],
                new_name: entry_name("another_module"),
                reference_actions: Vec::new(),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RenameModule(Err(RenameModuleError::Add(AddNamedChildError::AlreadyExists(_))))
        ));

        // The rename failed, so "beta" must still be there under its
        // original name — not silently removed then never restored.
        commands
            .send(Command::GetModulePools {
                module: vec![entry_name("beta")],
                request: 4,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::ModulePools(Some(_))));
    }

    #[tokio::test]
    async fn rename_module_that_does_not_exist_reports_not_found() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::RenameModule {
                target: vec![entry_name("does_not_exist")],
                new_name: entry_name("whatever"),
                reference_actions: Vec::new(),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::RenameModule(Err(RenameModuleError::NotFound))
        ));
    }

    #[tokio::test]
    async fn rename_module_with_reference_actions_repairs_a_descendant_reference() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // "beta" is a real submodule; put a requirement inside it that
        // the rename must be able to reach (module rename's "include
        // descendants" scope — see `reference_repair.rs`'s
        // `ReferenceTarget::Module` doc comment).
        commands
            .send(add_requirement_command(vec![entry_name("beta")], "inner", "Inner", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "outer", "Outer", 3))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::AddRequirement(Ok(()))));

        let mut dependent = RequirementDraft::new("Outer");
        dependent.requirement_text = "Text".to_string();
        dependent.dependencies.push(DependencyReferenceKind::RequirementReferenceV1(LocalGitReference {
            path: ReferencePath("/modules/beta/requirements/inner".to_string()),
            commit: "deadbeef".to_string(),
        }));
        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath::root(entry_name("outer")),
                requirement: Box::new(dependent),
                request: 4,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::UpdateRequirement(Ok(()))));

        let site = logical::ReferenceSite {
            referrer: LogicalPath::root(entry_name("outer")),
            kind: logical::ReferenceSiteKind::RequirementDependency { index: 0 },
        };
        commands
            .send(Command::RenameModule {
                target: vec![entry_name("beta")],
                new_name: entry_name("renamed_setup"),
                reference_actions: vec![(site, logical::ReferenceAction::Repair)],
                request: 5,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 5).await, Outcome::RenameModule(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("outer"))),
                request: 6,
            })
            .unwrap();
        match recv_completed(&mut events, 6).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { dependencies, .. })) => {
                assert!(matches!(
                    dependencies.as_slice(),
                    [DependencyReferenceKind::RequirementReferenceV1(local)]
                        if local.path.0 == "/modules/renamed_setup/requirements/inner"
                ));
            }
            other => panic!("expected EntryDetail(Some(Requirement)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn find_references_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::FindReferences {
                target: logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("design"))),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn find_references_returns_the_dependency_referencing_it() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "base", "Base", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "dependent", "Dependent", 3))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::AddRequirement(Ok(()))));

        let mut dependent = RequirementDraft::new("Dependent");
        dependent.requirement_text = "Text".to_string();
        dependent.dependencies.push(DependencyReferenceKind::RequirementReferenceV1(LocalGitReference {
            path: ReferencePath("/requirements/base".to_string()),
            commit: "deadbeef".to_string(),
        }));
        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath::root(entry_name("dependent")),
                requirement: Box::new(dependent),
                request: 4,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::UpdateRequirement(Ok(()))));

        commands
            .send(Command::FindReferences {
                target: logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("base"))),
                request: 5,
            })
            .unwrap();
        match recv_completed(&mut events, 5).await {
            Outcome::FindReferences(sites) => {
                assert_eq!(
                    sites,
                    vec![logical::ReferenceSite {
                        referrer: LogicalPath::root(entry_name("dependent")),
                        kind: logical::ReferenceSiteKind::RequirementDependency { index: 0 },
                    }]
                );
            }
            other => panic!("expected FindReferences, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn find_references_with_no_referrers_is_empty() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "lonely", "Lonely", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::FindReferences {
                target: logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("lonely"))),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::FindReferences(sites) if sites.is_empty()));
    }

    #[tokio::test]
    async fn repair_references_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::RepairReferences {
                old_target: logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("base"))),
                new_target: Some(logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("renamed")))),
                actions: Vec::new(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn repair_references_repairs_a_requirement_dependency_onto_the_new_target() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "base", "Base", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "renamed", "Renamed", 3))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "dependent", "Dependent", 4))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::AddRequirement(Ok(()))));

        let mut dependent = RequirementDraft::new("Dependent");
        dependent.requirement_text = "Text".to_string();
        dependent.dependencies.push(DependencyReferenceKind::RequirementReferenceV1(LocalGitReference {
            path: ReferencePath("/requirements/base".to_string()),
            commit: "deadbeef".to_string(),
        }));
        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath::root(entry_name("dependent")),
                requirement: Box::new(dependent),
                request: 5,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 5).await, Outcome::UpdateRequirement(Ok(()))));

        let site = logical::ReferenceSite {
            referrer: LogicalPath::root(entry_name("dependent")),
            kind: logical::ReferenceSiteKind::RequirementDependency { index: 0 },
        };
        commands
            .send(Command::RepairReferences {
                old_target: logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("base"))),
                new_target: Some(logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("renamed")))),
                actions: vec![(site, logical::ReferenceAction::Repair)],
                request: 6,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 6).await, Outcome::RepairReferences(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("dependent"))),
                request: 7,
            })
            .unwrap();
        match recv_completed(&mut events, 7).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { dependencies, .. })) => {
                assert!(matches!(
                    dependencies.as_slice(),
                    [DependencyReferenceKind::RequirementReferenceV1(local)]
                        if local.path.0 == "/requirements/renamed"
                ));
            }
            other => panic!("expected EntryDetail(Some(Requirement)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repair_references_with_repair_action_but_no_new_target_reports_the_error() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut referrer = RequirementDraft::new("Base");
        referrer.requirement_text = "Text".to_string();
        referrer.dependencies.push(DependencyReferenceKind::RequirementReferenceV1(LocalGitReference {
            path: ReferencePath("/requirements/base".to_string()),
            commit: "deadbeef".to_string(),
        }));
        commands
            .send(Command::AddRequirement {
                module: Vec::new(),
                name: entry_name("base"),
                requirement: Box::new(referrer),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        let site = logical::ReferenceSite {
            referrer: LogicalPath::root(entry_name("base")),
            kind: logical::ReferenceSiteKind::RequirementDependency { index: 0 },
        };
        commands
            .send(Command::RepairReferences {
                old_target: logical::ReferenceTarget::Requirement(LogicalPath::root(entry_name("base"))),
                new_target: None,
                actions: vec![(site, logical::ReferenceAction::Repair)],
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RepairReferences(Err(logical::ReferenceRepairError::RepairWithoutNewTarget))
        ));
    }

    #[tokio::test]
    async fn resolve_local_commit_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::ResolveLocalCommit {
                target: LogicalPath::root(entry_name("design")),
                kind: LocalCommitKind::Requirement,
                request: 1,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::ResolveLocalCommit(Err(ResolveLocalCommitError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn resolve_local_commit_after_loading_a_project_returns_gits_reply() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::ResolveLocalCommit {
                target: LogicalPath::root(entry_name("design")),
                kind: LocalCommitKind::Requirement,
                request: 2,
            })
            .unwrap();
        let Outcome::ResolveLocalCommit(result) = recv_completed(&mut events, 2).await else {
            panic!("expected ResolveLocalCommit");
        };
        // `FixedGit` (the default test double — see `spawn_test_actor`)
        // always answers "deadbeef" regardless of what path it's asked
        // about; `entry_directory`'s own path-building is covered
        // separately below, against `PathEchoingGit`.
        assert_eq!(result.unwrap(), "deadbeef");
    }

    #[tokio::test]
    async fn resolve_local_commit_builds_the_right_directory_for_a_nested_module() {
        let (commands, mut events) = spawn_test_actor_with_git(PathEchoingGit);

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // "beta" is a real submodule in `test_project` with no
        // requirements of its own yet — add one so there's a nested
        // target to resolve a commit for.
        commands
            .send(add_requirement_command(vec![entry_name("beta")], "marker", "Marker", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::ResolveLocalCommit {
                target: LogicalPath {
                    modules: vec![entry_name("beta")],
                    name: entry_name("marker"),
                },
                kind: LocalCommitKind::Requirement,
                request: 3,
            })
            .unwrap();
        let Outcome::ResolveLocalCommit(result) = recv_completed(&mut events, 3).await else {
            panic!("expected ResolveLocalCommit");
        };
        let expected = test_project_dir().join("modules/beta/requirements/marker");
        assert_eq!(result.unwrap(), expected.display().to_string());
    }

    #[tokio::test]
    async fn resolve_remote_commit_returns_gits_reply_without_needing_a_loaded_project() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::ResolveRemoteCommit {
                url: "https://example.com/repo.git".to_string(),
                path: Some(ReferencePath("some/path".to_string())),
                request: 1,
            })
            .unwrap();
        let Outcome::ResolveRemoteCommit(result) = recv_completed(&mut events, 1).await else {
            panic!("expected ResolveRemoteCommit");
        };
        assert_eq!(result.unwrap(), "deadbeef");
    }

    #[tokio::test]
    async fn get_changed_files_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands.send(Command::GetChangedFiles { request: 1 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::GetChangedFiles(Err(GetChangedFilesError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn get_changed_files_returns_gits_reply() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::GetChangedFiles { request: 2 }).unwrap();
        let Outcome::GetChangedFiles(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetChangedFiles");
        };
        // `FixedGit`'s own fixed reply — see its doc comment.
        assert_eq!(
            result.unwrap(),
            vec![PathBuf::from("root.txt"), PathBuf::from("sub/file.txt")]
        );
    }

    #[tokio::test]
    async fn get_diff_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::GetDiff {
                path: PathBuf::from("root.txt"),
                request: 1,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::GetDiff(Err(GetDiffError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn get_diff_returns_gits_reply() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetDiff {
                path: PathBuf::from("root.txt"),
                request: 2,
            })
            .unwrap();
        let Outcome::GetDiff(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetDiff");
        };
        // `FixedGit`'s own fixed reply — see its doc comment.
        assert_eq!(result.unwrap(), "diff for root.txt");
    }

    #[tokio::test]
    async fn get_commit_log_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::GetCommitLog {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                request: 1,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::GetCommitLog(Err(GetCommitLogError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn get_commit_log_returns_gits_reply() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetCommitLog {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                request: 2,
            })
            .unwrap();
        let Outcome::GetCommitLog(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetCommitLog");
        };
        // `FixedGit` doesn't override `commit_log_for_path`, so this
        // exercises the trait's own default impl built from
        // `commit_for_path_excluding`'s fixed "deadbeef" reply.
        let log = result.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].hash, "deadbeef");
    }

    #[tokio::test]
    async fn get_commit_log_for_a_result_resolves_the_nested_directory() {
        let (commands, mut events) = spawn_test_actor_with_git(PathEchoingGit);

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetCommitLog {
                target: EntryPath::Result(logical::ResultPath {
                    requirement: LogicalPath::root(entry_name("design")),
                    name: entry_name("some_result"),
                }),
                request: 2,
            })
            .unwrap();
        let Outcome::GetCommitLog(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetCommitLog");
        };
        // `PathEchoingGit` echoes back whatever path it was asked about
        // (via `commit_log_for_path`'s default impl, same as
        // `resolve_local_commit_builds_the_right_directory_for_a_nested_module`
        // does for `entry_directory`) — asserts `commit_log_directory`
        // built the result's own nested `results/<name>` directory, not
        // its owning requirement's.
        let expected = test_project_dir().join("requirements/design/results/some_result");
        assert_eq!(result.unwrap()[0].hash, expected.display().to_string());
    }

    #[tokio::test]
    async fn get_commit_files_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::GetCommitFiles {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                commit: "deadbeef".to_string(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::GetCommitFiles(Err(GetCommitFilesError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn get_commit_files_returns_gits_reply() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetCommitFiles {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                commit: "deadbeef".to_string(),
                request: 2,
            })
            .unwrap();
        let Outcome::GetCommitFiles(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetCommitFiles");
        };
        // `FixedGit` doesn't override `files_changed_in_commit`, so this
        // exercises the trait's own trivial default (`Ok(Vec::new())`).
        assert_eq!(result.unwrap(), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn get_commit_files_resolves_the_same_directory_as_get_commit_log() {
        let (commands, mut events) = spawn_test_actor_with_git(PathEchoingGit);

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetCommitFiles {
                target: EntryPath::Result(logical::ResultPath {
                    requirement: LogicalPath::root(entry_name("design")),
                    name: entry_name("some_result"),
                }),
                commit: "deadbeef".to_string(),
                request: 2,
            })
            .unwrap();
        let Outcome::GetCommitFiles(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetCommitFiles");
        };
        // `PathEchoingGit::files_changed_in_commit` echoes `dir` back —
        // same "proves `commit_log_directory` built the right nested
        // directory" reasoning as `get_commit_log_for_a_result_resolves_
        // the_nested_directory` above.
        let expected = test_project_dir().join("requirements/design/results/some_result");
        assert_eq!(result.unwrap(), vec![expected]);
    }

    #[tokio::test]
    async fn get_commit_file_diff_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::GetCommitFileDiff {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                commit: "deadbeef".to_string(),
                path: PathBuf::from("requirement.ron"),
                request: 1,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::GetCommitFileDiff(Err(GetCommitFileDiffError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn get_commit_file_diff_returns_gits_reply() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetCommitFileDiff {
                target: EntryPath::Requirement(LogicalPath::root(entry_name("design"))),
                commit: "deadbeef".to_string(),
                path: PathBuf::from("requirement.ron"),
                request: 2,
            })
            .unwrap();
        let Outcome::GetCommitFileDiff(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetCommitFileDiff");
        };
        // `FixedGit` doesn't override `diff_for_commit`, so this exercises
        // the trait's own default (delegates to `diff`, ignoring `commit`).
        assert_eq!(result.unwrap(), "diff for requirement.ron");
    }

    #[tokio::test]
    async fn get_commit_file_diff_resolves_the_same_directory_as_get_commit_log() {
        let (commands, mut events) = spawn_test_actor_with_git(PathEchoingGit);

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetCommitFileDiff {
                target: EntryPath::Result(logical::ResultPath {
                    requirement: LogicalPath::root(entry_name("design")),
                    name: entry_name("some_result"),
                }),
                commit: "deadbeef".to_string(),
                path: PathBuf::from("result.ron"),
                request: 2,
            })
            .unwrap();
        let Outcome::GetCommitFileDiff(result) = recv_completed(&mut events, 2).await else {
            panic!("expected GetCommitFileDiff");
        };
        let expected_dir = test_project_dir().join("requirements/design/results/some_result");
        assert_eq!(
            result.unwrap(),
            format!("{}:deadbeef:result.ron", expected_dir.display())
        );
    }

    #[tokio::test]
    async fn commit_all_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::CommitAll {
                message: "hello".to_string(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::CommitAll(Err(crate::CommitAllError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn commit_all_succeeds_against_a_loaded_project() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::CommitAll {
                message: "commit everything".to_string(),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::CommitAll(Ok(()))));
    }

    #[tokio::test]
    async fn push_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands.send(Command::Push { request: 1 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::Push(Err(crate::PushError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn push_succeeds_against_a_loaded_project() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::Push { request: 2 }).unwrap();
        match recv_completed(&mut events, 2).await {
            Outcome::Push(Ok(output)) => assert_eq!(output, "pushed"),
            other => panic!("expected Outcome::Push(Ok(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_unpushed_commits_without_a_loaded_project_reports_no_project_path() {
        let (commands, mut events) = spawn_test_actor();
        commands.send(Command::GetUnpushedCommits { request: 1 }).unwrap();
        assert!(matches!(
            recv_completed(&mut events, 1).await,
            Outcome::GetUnpushedCommits(Err(crate::GetUnpushedCommitsError::NoProjectPath))
        ));
    }

    #[tokio::test]
    async fn get_unpushed_commits_succeeds_against_a_loaded_project() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands.send(Command::GetUnpushedCommits { request: 2 }).unwrap();
        match recv_completed(&mut events, 2).await {
            Outcome::GetUnpushedCommits(Ok(commits)) => {
                assert_eq!(commits.len(), 1);
                assert_eq!(commits[0].subject, "a local commit");
            }
            other => panic!("expected Outcome::GetUnpushedCommits(Ok(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_module_summary_counts_recursively_and_reports_unvalidated() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::NewProject {
                name: "Scratch Project".to_string(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NewProject));

        commands
            .send(Command::AddModule {
                module: vec![],
                name: entry_name("sub"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddModule(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "r1", "R1", 3))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(add_requirement_command(vec![entry_name("sub")], "r2", "R2", 4))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::AddRequirement(Ok(()))));

        let result = logical::draft::ResultDraft::new(
            "Res1",
            "deadbeef",
            ReferencePath("/tests/does_not_exist".to_string()),
            "deadbeef",
        );
        commands
            .send(Command::AddResult {
                requirement: LogicalPath::root(entry_name("r1")),
                name: entry_name("res1"),
                result: Box::new(result),
                request: 5,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 5).await, Outcome::AddResult(Ok(()))));

        commands
            .send(Command::GetModuleSummary {
                module: vec![],
                request: 6,
            })
            .unwrap();
        match recv_completed(&mut events, 6).await {
            Outcome::ModuleSummary(Some(summary)) => {
                assert_eq!(summary.submodule_count, 1);
                assert_eq!(summary.requirement_count, 2);
                assert_eq!(summary.result_count, 1);
                assert_eq!(summary.test_count, 0);
                // `ResultDraft::new` defaults to `StatusV1::Incomplete`.
                assert_eq!(summary.results_incomplete, 1);
                assert_eq!(summary.results_pass, 0);
                assert_eq!(summary.results_fail, 0);
                assert!(!summary.validated);
                assert_eq!(summary.requirements_met, 0);
                assert_eq!(summary.requirements_unmet, 0);
            }
            other => panic!("expected ModuleSummary(Some(_)), got {other:?}"),
        }

        // The submodule's own summary only covers its own subtree.
        commands
            .send(Command::GetModuleSummary {
                module: vec![entry_name("sub")],
                request: 7,
            })
            .unwrap();
        match recv_completed(&mut events, 7).await {
            Outcome::ModuleSummary(Some(summary)) => {
                assert_eq!(summary.submodule_count, 0);
                assert_eq!(summary.requirement_count, 1);
                assert_eq!(summary.result_count, 0);
            }
            other => panic!("expected ModuleSummary(Some(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_module_summary_for_a_missing_module_reports_none() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::GetModuleSummary {
                module: vec![entry_name("does_not_exist")],
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::ModuleSummary(None)));
    }

    #[tokio::test]
    async fn rename_project_updates_the_root_name() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::RenameProject {
                new_name: "Renamed Project".to_string(),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::RenameProject(Ok(()))));

        let snapshot = recv_tree_changed(&mut events).await;
        assert_eq!(snapshot.root.name, entry_name("Renamed Project"));
    }

    #[tokio::test]
    async fn rename_project_rejects_an_empty_name() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: test_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::RenameProject {
                new_name: "   ".to_string(),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::RenameProject(Err(RenameProjectError::EmptyName))
        ));
    }

    #[tokio::test]
    async fn rename_project_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::RenameProject {
                new_name: "Whatever".to_string(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }
}
