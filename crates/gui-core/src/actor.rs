//! The actor: owns `ProjectState` exclusively, dispatches `Command`s,
//! enforces the one-mutation-at-a-time / concurrent-reads policy. See
//! `README.md`'s "The actor + spawn-per-request pattern" and "Exactly one
//! mutation at a time" sections.

use std::collections::VecDeque;
use std::path::PathBuf;

use syscalls::{Filesystem, Git, RemoteGit};
use tokio::sync::mpsc;

use crate::tree::{
    build_tree_snapshot, dependency_chain, get_entry_detail, get_module_pools, is_requirement_met, resolve_module_mut,
};
use crate::{
    AddChildError, AddLocalPoolError, AddPoolChildError, AddPoolFileError, Command, Event, Outcome, ProjectState,
    RedoError, RenameModuleError, RequestId, SaveError, UndoError, UpdateChildError,
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
    pub fn start() -> CoreHandle {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .expect("failed to start gui-core's tokio runtime");

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        runtime.spawn(run_actor(
            command_rx,
            event_tx,
            syscalls::StdFilesystem,
            syscalls::SystemGit,
        ));

        CoreHandle {
            commands: command_tx,
            events: event_rx,
            _runtime: runtime,
        }
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
    pub fn start_with<F, G>(fs: F, git: G) -> CoreHandle
    where
        F: Filesystem + Clone + Send + Sync + 'static,
        G: Git + RemoteGit + Clone + Send + Sync + 'static,
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .expect("failed to start gui-core's tokio runtime");

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        runtime.spawn(run_actor(command_rx, event_tx, fs, git));

        CoreHandle {
            commands: command_tx,
            events: event_rx,
            _runtime: runtime,
        }
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
                module,
                name,
                result,
                request,
            } => self.add_result(module, name, result, request),
            Command::UpdateResult { target, result, request } => self.update_result(target, result, request),
            Command::RemoveResult { target, request } => self.remove_result(target, request),
            Command::AddModule { module, name, request } => self.add_module(module, name, request),
            Command::RemoveModule { target, request } => self.remove_module(target, request),
            Command::RenameModule {
                target,
                new_name,
                request,
            } => self.rename_module(target, new_name, request),
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
            Command::GetEntryDetail { target, kind, request } => {
                self.spawn_read(request, move |state| get_entry_detail(&state, &target, kind))
            }
            Command::IsRequirementMet { target, request } => {
                self.spawn_read(request, move |state| is_requirement_met(&state, &target))
            }
            Command::DependencyChain { target, request } => {
                self.spawn_read(request, move |state| dependency_chain(&state, &target))
            }
            Command::GetModulePools { module, request } => {
                self.spawn_read(request, move |state| get_module_pools(&state, &module))
            }
            Command::Shutdown => unreachable!("handled in run_actor's select! before dispatch is called"),
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
        if self.state.is_none() {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        }
        self.push_undo_snapshot();
        self.ensure_draft();
        let ProjectState::Draft(draft) = self.state.as_mut().expect("just ensured Some(Draft)") else {
            unreachable!("ensure_draft leaves state as Draft")
        };
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
        if self.state.is_none() {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        }
        let undo_snapshot = self.snapshot_state();
        self.ensure_draft();
        let ProjectState::Draft(draft) = self.state.as_mut().expect("just ensured Some(Draft)") else {
            unreachable!("ensure_draft leaves state as Draft")
        };
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
    /// responsible for the `NotFound` check (each kind's own `BTreeMap`),
    /// there's no `logical`-level "update" call to delegate to, unlike
    /// `add_*`/`remove_*`. Only fires `TreeChanged` when `f` actually
    /// changed something, same reasoning as `remove_from_module`.
    fn update_in_module(
        &mut self,
        request: RequestId,
        module: &[disk::EntryName],
        f: impl FnOnce(&mut logical::draft::ModuleDraft) -> Result<(), UpdateChildError>,
        outcome: impl FnOnce(Result<(), UpdateChildError>) -> Outcome,
    ) {
        if self.state.is_none() {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        }
        let undo_snapshot = self.snapshot_state();
        self.ensure_draft();
        let ProjectState::Draft(draft) = self.state.as_mut().expect("just ensured Some(Draft)") else {
            unreachable!("ensure_draft leaves state as Draft")
        };
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
            move |module| {
                if let std::collections::btree_map::Entry::Occupied(mut e) = module.requirements.entry(name) {
                    e.insert(*requirement);
                    Ok(())
                } else {
                    Err(UpdateChildError::NotFound)
                }
            },
            Outcome::UpdateRequirement,
        );
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
            move |module| {
                if let std::collections::btree_map::Entry::Occupied(mut e) = module.tests.entry(name) {
                    e.insert(*test);
                    Ok(())
                } else {
                    Err(UpdateChildError::NotFound)
                }
            },
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
        module: Vec<disk::EntryName>,
        name: disk::EntryName,
        result: Box<logical::draft::ResultDraft>,
        request: RequestId,
    ) {
        self.mutate_module(
            request,
            &module,
            || Outcome::AddResult(Err(AddChildError::ModuleNotFound)),
            move |target_module| {
                Outcome::AddResult(
                    target_module
                        .add_result(name.as_str(), *result)
                        .map_err(AddChildError::Add),
                )
            },
        );
    }

    fn update_result(
        &mut self,
        target: logical::LogicalPath,
        result: Box<logical::draft::ResultDraft>,
        request: RequestId,
    ) {
        let logical::LogicalPath { modules, name } = target;
        self.update_in_module(
            request,
            &modules,
            move |module| {
                if let std::collections::btree_map::Entry::Occupied(mut e) = module.results.entry(name) {
                    e.insert(*result);
                    Ok(())
                } else {
                    Err(UpdateChildError::NotFound)
                }
            },
            Outcome::UpdateResult,
        );
    }

    fn remove_result(&mut self, target: logical::LogicalPath, request: RequestId) {
        let logical::LogicalPath { modules, name } = target;
        self.remove_from_module(
            request,
            &modules,
            move |module| module.remove_result(name.as_str()).is_some(),
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
    fn rename_module(&mut self, target: Vec<disk::EntryName>, new_name: disk::EntryName, request: RequestId) {
        let Some((old_name, parent_path)) = target.split_last() else {
            self.complete(request, Outcome::RenameModule(Err(RenameModuleError::CannotRenameRoot)));
            return;
        };
        let old_name = old_name.clone();
        self.mutate_module(
            request,
            parent_path,
            || Outcome::RenameModule(Err(RenameModuleError::ModuleNotFound)),
            move |parent| {
                if !parent.modules.contains_key(&old_name) {
                    return Outcome::RenameModule(Err(RenameModuleError::NotFound));
                }
                if let Err(err) = parent.add_module(new_name.as_str()) {
                    return Outcome::RenameModule(Err(RenameModuleError::Add(err)));
                }
                let renamed = parent.modules.remove(&old_name).expect("just confirmed present above");
                parent.modules.insert(new_name, renamed);
                Outcome::RenameModule(Ok(()))
            },
        );
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
    /// entry, not the module as a whole), then apply `f`.
    fn add_local_pool<T>(
        &mut self,
        request: RequestId,
        target: logical::LogicalPath,
        get_entry: impl for<'a> FnOnce(&'a mut logical::draft::ModuleDraft, &disk::EntryName) -> Option<&'a mut T>,
        f: impl FnOnce(&mut T) -> Result<(), AddPoolFileError>,
        outcome: impl FnOnce(Result<(), AddLocalPoolError>) -> Outcome,
    ) {
        if self.state.is_none() {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        }
        let undo_snapshot = self.snapshot_state();
        self.ensure_draft();
        let ProjectState::Draft(draft) = self.state.as_mut().expect("just ensured Some(Draft)") else {
            unreachable!("ensure_draft leaves state as Draft")
        };
        let result = match resolve_module_mut(&mut draft.tree, &target.modules) {
            None => Err(AddLocalPoolError::ModuleNotFound),
            Some(module) => match get_entry(module, &target.name) {
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
        target: logical::LogicalPath,
        get_entry: impl for<'a> FnOnce(&'a mut logical::draft::ModuleDraft, &disk::EntryName) -> Option<&'a mut T>,
        f: impl FnOnce(&mut T) -> bool,
        outcome: impl FnOnce(bool) -> Outcome,
    ) {
        if self.state.is_none() {
            self.complete(request, Outcome::NoProjectLoaded);
            return;
        }
        let undo_snapshot = self.snapshot_state();
        self.ensure_draft();
        let ProjectState::Draft(draft) = self.state.as_mut().expect("just ensured Some(Draft)") else {
            unreachable!("ensure_draft leaves state as Draft")
        };
        let removed = match resolve_module_mut(&mut draft.tree, &target.modules) {
            None => false,
            Some(module) => match get_entry(module, &target.name) {
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
            target,
            |module, name| module.requirements.get_mut(name),
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
            target,
            |module, name| module.requirements.get_mut(name),
            move |requirement: &mut logical::draft::RequirementDraft| requirement.remove_attachment(&path),
            Outcome::RemoveRequirementAttachment,
        );
    }

    fn add_test_attachment(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.add_local_pool(
            request,
            target,
            |module, name| module.tests.get_mut(name),
            move |test: &mut logical::draft::TestDraft| test.add_attachment(&path),
            Outcome::AddTestAttachment,
        );
    }

    fn remove_test_attachment(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.remove_local_pool(
            request,
            target,
            |module, name| module.tests.get_mut(name),
            move |test: &mut logical::draft::TestDraft| test.remove_attachment(&path),
            Outcome::RemoveTestAttachment,
        );
    }

    fn add_test_template_file(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.add_local_pool(
            request,
            target,
            |module, name| module.tests.get_mut(name),
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
            target,
            |module, name| module.tests.get_mut(name),
            move |test: &mut logical::draft::TestDraft| test.remove_template_file(&path),
            Outcome::RemoveTestTemplateFile,
        );
    }

    fn add_result_attachment(&mut self, target: logical::LogicalPath, path: std::path::PathBuf, request: RequestId) {
        self.add_local_pool(
            request,
            target,
            |module, name| module.results.get_mut(name),
            move |result: &mut logical::draft::ResultDraft| result.add_attachment(&path),
            Outcome::AddResultAttachment,
        );
    }

    fn remove_result_attachment(
        &mut self,
        target: logical::LogicalPath,
        path: std::path::PathBuf,
        request: RequestId,
    ) {
        self.remove_local_pool(
            request,
            target,
            |module, name| module.results.get_mut(name),
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
        tokio::task::spawn_blocking(move || {
            let outcome = match &state {
                ProjectState::Validated(validated) => {
                    Outcome::Save(validated.save(&fs, &dir).map_err(SaveError::Save))
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
        tokio::task::spawn_blocking(move || {
            let (outcome, saved_path) = match &state {
                ProjectState::Validated(validated) => match validated.save(&fs, &path) {
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

#[cfg(test)]
mod test {
    use std::path::{Path, PathBuf};

    use disk::{EntryName, LocalGitReference, ReferencePath, TestReferenceKind};
    use logical::LogicalPath;
    use logical::draft::RequirementDraft;
    use syscalls::{CommitForPathError, CommitForRemoteError, StdFilesystem};

    use logical::draft::AddNamedChildError;

    use crate::{AddPoolFileError, EntryDetail, EntryKind, TreeSnapshot};

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
    }

    impl syscalls::RemoteGit for FixedGit {
        fn commit_for_remote(&self, _url: &str, _path: Option<&Path>) -> Result<String, CommitForRemoteError> {
            Ok("deadbeef".to_string())
        }
    }

    fn sample_project_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project")
    }

    /// A writable copy of `sample_project`, so a `Save` test can actually
    /// write to disk without touching the repository's own fixture. Named
    /// per test/process/line, same convention `disk`'s own tests use for
    /// scratch directories (see `crates/disk/src/project/operations/
    /// load.rs`'s tests) — caller is responsible for `remove_dir_all`.
    fn scratch_copy_of_sample_project(label: &str) -> PathBuf {
        let dest = std::env::temp_dir().join(format!("gui-core-actor-test-{label}-{}", std::process::id()));
        std::fs::remove_dir_all(&dest).ok();
        let status = std::process::Command::new("cp")
            .args(["-r", sample_project_dir().to_str().unwrap(), dest.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success(), "failed to copy sample_project to {dest:?}");
        dest
    }

    fn entry_name(name: &str) -> EntryName {
        EntryName(name.to_string())
    }

    fn spawn_test_actor() -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_actor(command_rx, event_tx, StdFilesystem, FixedGit));
        (command_tx, event_rx)
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

    fn add_requirement_command(module: Vec<EntryName>, name: &str, title: &str, request: RequestId) -> Command {
        Command::AddRequirement {
            module,
            name: entry_name(name),
            requirement: Box::new(RequirementDraft::new(title)),
            request,
        }
    }

    #[tokio::test]
    async fn add_requirement_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(add_requirement_command(vec![], "definition", "Definition", 1))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::NoProjectLoaded));
    }

    #[tokio::test]
    async fn get_entry_detail_without_a_loaded_project_reports_no_project_loaded() {
        let (commands, mut events) = spawn_test_actor();
        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath::root(entry_name("definition")),
                kind: EntryKind::Requirement,
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
                path: sample_project_dir(),
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
                target: LogicalPath::root(entry_name("scratch")),
                kind: EntryKind::Requirement,
                request: 3,
            })
            .unwrap();
        match recv_completed(&mut events, 3).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { title, .. })) => {
                assert_eq!(title, "Scratch")
            }
            other => panic!("expected EntryDetail(Some(_)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_project_then_add_requirement_with_a_dependency_then_read_it_back() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut requirement = RequirementDraft::new("Scratch");
        requirement.dependencies.push(disk::DependencyReferenceKind::RequirementReferenceV1(
            disk::LocalGitReference {
                path: disk::ReferencePath("/requirements/definition".to_string()),
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
                target: LogicalPath::root(entry_name("scratch")),
                kind: EntryKind::Requirement,
                request: 3,
            })
            .unwrap();
        match recv_completed(&mut events, 3).await {
            Outcome::EntryDetail(Some(EntryDetail::Requirement { dependencies, .. })) => {
                assert_eq!(dependencies.len(), 1);
                assert!(matches!(
                    &dependencies[0],
                    disk::DependencyReferenceKind::RequirementReferenceV1(local)
                        if local.path.0 == "/requirements/definition" && local.commit == "abc123"
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                target: LogicalPath::root(entry_name("after_validate")),
                kind: EntryKind::Requirement,
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

    /// `logical::validate::validate` drops the draft on `Err` (see the
    /// comment in `spawn_validate`) — this confirms the clone-before-
    /// validate restore actually works end to end: the broken requirement
    /// is still there, still editable, after a failed `Validate`.
    #[tokio::test]
    async fn a_failed_validate_restores_an_editable_draft() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let mut broken = RequirementDraft::new("Broken");
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
                target: LogicalPath::root(entry_name("broken")),
                kind: EntryKind::Requirement,
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
        let dir = scratch_copy_of_sample_project("save-writes-to-disk");
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
                path: sample_project_dir(),
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

        // The old project's "definition" requirement is gone — this
        // really replaced the state, not merged into it.
        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath::root(entry_name("definition")),
                kind: EntryKind::Requirement,
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                target: LogicalPath::root(entry_name("scratch")),
                kind: EntryKind::Requirement,
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::EntryDetail(Some(_))));

        commands.send(Command::Undo { request: 4 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::Undo(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath::root(entry_name("scratch")),
                kind: EntryKind::Requirement,
                request: 5,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 5).await, Outcome::EntryDetail(None)));

        commands.send(Command::Redo { request: 6 }).unwrap();
        assert!(matches!(recv_completed(&mut events, 6).await, Outcome::Redo(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath::root(entry_name("scratch")),
                kind: EntryKind::Requirement,
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
        let dir = scratch_copy_of_sample_project("save-as-failure-keeps-path");
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddTest {
                module: vec![],
                name: entry_name("scratch_test"),
                test: Box::new(logical::draft::TestDraft::new("Scratch Test", disk::ResultKindV1::FreeForm)),
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let result = logical::draft::ResultDraft::new(
            "Scratch Result",
            ReferencePath("/requirements/definition".to_string()),
            "deadbeef",
            ReferencePath("/tests/generic_test".to_string()),
            "deadbeef",
        );
        commands
            .send(Command::AddResult {
                module: vec![],
                name: entry_name("scratch_result"),
                result: Box::new(result),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddResult(Ok(()))));

        commands
            .send(Command::RemoveResult {
                target: LogicalPath::root(entry_name("scratch_result")),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(add_requirement_command(vec![], "scratch", "Original", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath::root(entry_name("scratch")),
                requirement: Box::new(RequirementDraft::new("Updated")),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::UpdateRequirement(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath::root(entry_name("scratch")),
                kind: EntryKind::Requirement,
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::UpdateRequirement {
                target: LogicalPath::root(entry_name("does_not_exist")),
                requirement: Box::new(RequirementDraft::new("Updated")),
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
                path: sample_project_dir(),
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
    async fn update_test_replaces_content() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddTest {
                module: vec![],
                name: entry_name("scratch_test"),
                test: Box::new(logical::draft::TestDraft::new("Original", disk::ResultKindV1::FreeForm)),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddTest(Ok(()))));

        commands
            .send(Command::UpdateTest {
                target: LogicalPath::root(entry_name("scratch_test")),
                test: Box::new(logical::draft::TestDraft::new("Updated", disk::ResultKindV1::Template)),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::UpdateTest(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath::root(entry_name("scratch_test")),
                kind: EntryKind::Test,
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let original = logical::draft::ResultDraft::new(
            "Original",
            ReferencePath("/requirements/definition".to_string()),
            "deadbeef",
            ReferencePath("/tests/generic_test".to_string()),
            "deadbeef",
        );
        commands
            .send(Command::AddResult {
                module: vec![],
                name: entry_name("scratch_result"),
                result: Box::new(original),
                request: 2,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddResult(Ok(()))));

        let updated = logical::draft::ResultDraft::new(
            "Updated",
            ReferencePath("/requirements/discovery".to_string()),
            "cafef00d",
            ReferencePath("/tests/generic_test".to_string()),
            "cafef00d",
        );
        commands
            .send(Command::UpdateResult {
                target: LogicalPath::root(entry_name("scratch_result")),
                result: Box::new(updated),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::UpdateResult(Ok(()))));

        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath::root(entry_name("scratch_result")),
                kind: EntryKind::Result,
                request: 4,
            })
            .unwrap();
        match recv_completed(&mut events, 4).await {
            Outcome::EntryDetail(Some(EntryDetail::Result { title, requirement_path, .. })) => {
                assert_eq!(title, "Updated");
                assert_eq!(requirement_path, "/requirements/discovery");
            }
            other => panic!("expected EntryDetail(Some(Result)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_then_remove_attachment_round_trips() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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

    /// `sample_project`'s root `attachments/glossary.md` is already
    /// physically present, so loading the project already populates the
    /// root module's pool with it — adding it again should collide.
    #[tokio::test]
    async fn add_attachment_rejects_a_duplicate_path() {
        let (commands, mut events) = spawn_test_actor();

        commands
            .send(Command::LoadProject {
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::AddAttachment {
                module: vec![],
                path: PathBuf::from("glossary.md"),
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
                path: sample_project_dir(),
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
                // The root already carries glossary.md (physically present
                // in sample_project) plus the one just added.
                assert!(pools.attachments.contains(&PathBuf::from("glossary.md")));
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = LogicalPath::root(entry_name("definition"));
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
                target: target.clone(),
                kind: EntryKind::Requirement,
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = LogicalPath::root(entry_name("generic_test"));
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = LogicalPath::root(entry_name("generic_test"));
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        let target = LogicalPath::root(entry_name("definition"));
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        // "setup" is a real submodule in sample_project — add a
        // requirement into it first, to prove the rename doesn't lose
        // the module's existing content.
        commands
            .send(add_requirement_command(vec![entry_name("setup")], "marker", "Marker", 2))
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 2).await, Outcome::AddRequirement(Ok(()))));

        commands
            .send(Command::RenameModule {
                target: vec![entry_name("setup")],
                new_name: entry_name("renamed_setup"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 3).await, Outcome::RenameModule(Ok(()))));

        // The old name is gone...
        commands
            .send(Command::GetModulePools {
                module: vec![entry_name("setup")],
                request: 4,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 4).await, Outcome::ModulePools(None)));

        // ...and the new name has the module's original content.
        commands
            .send(Command::GetEntryDetail {
                target: LogicalPath {
                    modules: vec![entry_name("renamed_setup")],
                    name: entry_name("marker"),
                },
                kind: EntryKind::Requirement,
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::RenameModule {
                target: vec![],
                new_name: entry_name("whatever"),
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
                path: sample_project_dir(),
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
                target: vec![entry_name("setup")],
                new_name: entry_name("another_module"),
                request: 3,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 3).await,
            Outcome::RenameModule(Err(RenameModuleError::Add(AddNamedChildError::AlreadyExists(_))))
        ));

        // The rename failed, so "setup" must still be there under its
        // original name — not silently removed then never restored.
        commands
            .send(Command::GetModulePools {
                module: vec![entry_name("setup")],
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
                path: sample_project_dir(),
                request: 1,
            })
            .unwrap();
        assert!(matches!(recv_completed(&mut events, 1).await, Outcome::LoadProject(Ok(()))));

        commands
            .send(Command::RenameModule {
                target: vec![entry_name("does_not_exist")],
                new_name: entry_name("whatever"),
                request: 2,
            })
            .unwrap();
        assert!(matches!(
            recv_completed(&mut events, 2).await,
            Outcome::RenameModule(Err(RenameModuleError::NotFound))
        ));
    }
}
