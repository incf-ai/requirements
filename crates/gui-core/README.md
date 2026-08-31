# gui-core

**Status: the actor is implemented** — `CoreHandle`, `ProjectState`,
`Command`/`Event`/`Outcome`, and the actor's dispatch/exclusion/completion
logic in `src/actor.rs` all match this README, with tests in `actor.rs`'s
own `test` module covering the mutation-exclusion queue, `Draft`↔
`Validated` demotion, a failed `Validate`'s draft-restore, and a real
`Save` round-trip against a scratch copy of `sample_project`.

The full `Command` surface, by family — each sharing one error type across
its siblings the way `AddNamedChildError` already does in `logical` (see
each type's own doc comment for the specific reasoning):
- **Add/Update/Remove** for all four named-child kinds (requirement/test/
  result/module) — `AddChildError`/`UpdateChildError` shared across each
  family; `EntryDetail` carries full per-kind editable fields (not just a
  title) so `gui-ui` can build an `Update*` straight from a
  `GetEntryDetail` reply.
- **Module-level attachment/template** add/remove, addressed by `module` +
  `path` (a `BTreeSet<PathBuf>` pool, not a named-child map), sharing
  `AddPoolChildError`; `GetModulePools` is the matching on-demand read.
- **A requirement/test/result's own *local* attachment pools** (plus a
  test's separate local *template* pool — `logical::draft::TestDraft`
  keeps that distinct from its attachments) — 8 commands addressed by the
  entry's own `target: LogicalPath`, sharing `AddLocalPoolError`.
  `EntryDetail` carries each entry's local pool contents too.
- **`RenameModule`** — renames a module in place (same parent, all content
  preserved), reusing `ModuleDraft::add_module`'s own sanitize/collision
  check by creating-then-overwriting rather than duplicating that logic;
  its own `RenameModuleError` isn't shared with anything, since there's no
  sibling `rename_*` command family for a requirement/test/result (their
  `update_*` already replaces the whole entry in one step instead).

`gui-ui`'s forms do double duty as create-or-edit, its three editable
forms each manage their own local-attachment pools end to end, its
Attachments dialog exercises the module-level pools, and its tree has a
rename button per module — see its own README.

## What this crate is for

`logical` gives us `ProjectDraft`/`ValidatedProject` as plain, synchronous,
owned data — no interior mutability, no async, no threads. `gui-core` is the
layer that turns that into something a GUI can talk to: it owns the loaded
project's state, runs all the work that can be slow (disk IO via `disk`,
remote git resolution via `syscalls::RemoteGit` during `validate()`) on a
multi-threaded async runtime, and exposes that state and those operations to
`gui-ui` purely through message passing over channels — never a shared
`Arc<Mutex<_>>`, never a direct function call across the thread boundary.

`gui-ui` is one OS thread running an immediate-mode UI loop that must never
block. `gui-core` is where blocking, slow, or fallible work is allowed to
happen, isolated so that it never stalls the UI, and structured so that one
slow operation doesn't stall every *other* operation either.

## Threading model

`gui-core` owns a `tokio::runtime::Runtime` built with
`Builder::new_multi_thread()` — it is the only thing in the process that
knows about tokio. `gui-ui` is not async at all; it never calls `.await` and
never links against a tokio runtime of its own. The boundary between the two
is `CoreHandle`, a plain `Send + Sync` struct gui-core hands back to whoever
starts it up:

```rust
pub struct CoreHandle {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    // keeps the Runtime (and its worker threads) alive for the process
    // lifetime; dropping it would shut the runtime down.
    _runtime: tokio::runtime::Runtime,
}

impl CoreHandle {
    pub fn start() -> CoreHandle;
    pub fn start_with<F, G>(fs: F, git: G) -> CoreHandle
    where
        F: Filesystem + Clone + Send + Sync + 'static,
        G: Git + RemoteGit + Clone + Send + Sync + 'static;
    pub fn send(&self, command: Command);
    /// Non-blocking. Called once per UI frame.
    pub fn try_recv_event(&mut self) -> Option<Event>;
}
```

`start()` builds the runtime, spawns the actor (below) onto it, and returns
immediately — `gui-ui`'s `main()` calls this once, then runs its own
(blocking, synchronous) UI event loop on the calling thread for the rest of
the process's life, polling `try_recv_event` once per frame. Nothing on the
UI thread ever awaits or blocks on a channel recv with no timeout — see
"Why this can't deadlock" below.

`start_with` is the same actor loop against caller-supplied `Filesystem`/
`Git`/`RemoteGit` implementations instead of the real `syscalls::
StdFilesystem`/`syscalls::SystemGit` — test-support surface for `gui-ui`'s
own `egui_kittest` tests, which run against a real `CoreHandle` (not a
fake actor) but sometimes need to control something `start()` can't, like
making a real `Save` artificially slow via `syscalls::SlowFilesystem` to
exercise the exit dialog's Saving/TimedOut states deterministically — see
that crate's README for the fuller story of why that needed a real hook
rather than just a short timeout.

### Project state: alternating Draft/Validated, one owner

The actor owns exactly one project at a time, and it's always in one of the
two shapes `logical` defines — never both, never neither once a project is
loaded:

```rust
enum ProjectState {
    Draft(logical::ProjectDraft),
    Validated(logical::ValidatedProject),
}
```

This mirrors `logical`'s own "two types, not one" design (see its README)
rather than inventing a third gui-core-specific state — `gui-core` doesn't
get to treat "validated" as a bit it can set and clear; the only way in is
`ProjectDraft::validate()`, the only way back to an editable state is
`ValidatedProject::into_draft()`, same as any other caller of `logical`.
Concretely: an edit command (`add_requirement`, etc.) arriving while the
actor holds `ProjectState::Validated` first calls `into_draft()` — an
implicit, automatic demotion, not an error — *then* applies the edit. The
project loses its resolved-reference data at that point and stays a `Draft`
until the next explicit `Validate` command; this is expected, not a bug to
route around, since `logical` guarantees an edited project can't stay
validated without being re-checked anyway.

### The actor + spawn-per-request pattern

A single async task — "the actor" — owns `ProjectState` exclusively.
Nothing else touches it. The actor's loop is a `tokio::select!` over two
channels, not one:

```rust
loop {
    tokio::select! {
        Some(command) = commands.recv() => { /* handle or dispatch */ }
        Some(completion) = completions.recv() => { /* apply finished work, clear mutation_in_flight */ }
    }
}
```

- **Cheap, purely local mutations** (`add_requirement`, `remove_test`, ...)
  are handled inline, synchronously, in the `select!` arm itself — these are
  exactly the `logical` operations the "Operations catalog" in `logical`'s
  README documents as never failing on cross-entity grounds and never doing
  IO. They finish in microseconds, so "checking out" the state for one of
  these and handing it back happens within the same `select!` iteration —
  nothing else ever actually observes them as in flight (see "Exactly one
  mutation at a time" below).
- **Anything that can be slow** (`validate()` — may hit the network per
  `logical`'s remote-dependency resolution; `save()` — disk IO; loading a
  project from a path) is *not* run inline. The actor takes the owned
  `ProjectState` it needs (plain data, so this is a move, not a
  clone-and-diverge), `tokio::spawn`s a task that does the work, and
  immediately goes back to `select!` — free to keep servicing *read*
  commands against the last-known-good state while that task runs (see
  below). When the spawned task finishes, it sends its result back over the
  `completions` channel (a plain `mpsc`, not a `oneshot`, so the actor loop
  is what observes completions, not some other part of the program) and the
  actor applies it: swap in the new `ProjectState`, or leave the old one in
  place and report the error.

**Why not just `Arc<Mutex<ProjectState>>` shared with the spawned tasks
instead?** Two reasons. First, it reintroduces exactly the shared mutable
state `logical` was designed to avoid (see `logical`'s README: "this crate
should stay in plain owned data, no shared mutable state"). Second, and more
concretely: a `Mutex` guarding a long `validate()` call would just move the
deadlock-shaped problem from "message passing" to "lock contention" — a
second command needing the lock would block the thread trying to acquire it
instead of blocking on a channel, which is worse, not better, since a
blocked-on-mutex task ties up a tokio worker thread that could otherwise be
servicing unrelated work. Moving ownership into the spawned task and getting
it back via a completion message keeps the actor as the single owner at
every point in time, and keeps "who currently holds the state" always
answerable by reading the actor's own local variable, not by reasoning about
lock state.

### Exactly one mutation at a time; reads run freely alongside each other

**Decided policy**: at most one mutating command is being handled at any
moment — for its entire duration, no other command, mutating *or*
read-only, is dispatched. Once no mutation is in flight, any number of
read-only commands can be dispatched and run concurrently with each other.
Concretely:

```rust
struct Actor {
    state: ProjectState,
    mutation_in_flight: bool,
    queue: VecDeque<Command>,
}
```

- A **mutating** command (`AddRequirement`, `RemoveTest`, `Validate`,
  `Save`, `LoadProject`, ...) arriving while `mutation_in_flight` is `true`
  goes straight to `queue` — not rejected with `Event::Busy`, since nothing
  about "add a requirement while a save is running" should be an error the
  user has to retry by hand. Arriving while `mutation_in_flight` is `false`,
  it sets the flag, checks out `state` (moved into a spawned task, or
  handled inline and the flag cleared immediately for a cheap local edit —
  either way `mutation_in_flight` is `true` for the operation's actual
  duration, whether that's microseconds or a slow `validate()`), and *while
  it's set, every other command, including reads, queues behind it too.*
  This is the "to the exclusion of all other actions" half of the policy:
  a read started concurrently with, say, `save()` could observe a
  half-written project, so reads don't get a special exemption from a
  mutation's exclusivity — only from *each other's*.
- A **read-only** command (tree snapshot for the left pane, `is_requirement
  _met`, `dependency_chain`, entry detail for the center pane, ...)
  arriving while `mutation_in_flight` is `false` is dispatched immediately,
  each as its own spawned task operating on its own `state.clone()`. Any
  number of these can be in flight together — none of them mutate, so
  there's nothing for them to race on, and no reason to serialize them.
- When a mutation's completion arrives, the actor applies it, clears
  `mutation_in_flight`, then drains `queue` in arrival order — front to
  back, re-running the same dispatch logic per entry, which naturally
  re-pauses at the next mutating command if one is queued behind reads that
  can now run immediately.

This is a stronger rule than "just don't corrupt `state`" would strictly
require — two reads truly never conflict with each other regardless of
what's running, so only "mutation vs. anything" needs serializing, not
"anything vs. anything." Also covers the earlier `add_requirement`-during-
`validate()` case as one instance of the general rule, rather than a special
case: `add_requirement` is a mutation, `validate()` is a mutation, so the
second queues behind the first, full stop.

**`Arc` and `Mutex` are both off the table, by rule, not just by
default.** `ProjectState` is always moved or `clone()`d, never wrapped in
either. This is a stronger constraint than "whichever is faster": an `Arc`
around `state` would let a spawned read task hold a reference into data the
actor might swap out from under it (needing a `Mutex` to make that safe
again, or `ArcSwap`/similar to dodge it — machinery this design specifically
avoids, see "Why not `Arc<Mutex<ProjectState>>`" above), and a `Mutex`
reintroduces lock contention as a blocking hazard on a tokio worker thread.
Plain `clone()` keeps every task's data fully independent and owned, so
"can this deadlock or race" is answered by the type system (no shared
references exist to race over) rather than by correct lock discipline. If
`ProjectState::clone()` turns out to be too slow at real project sizes,
the fix is a cheaper *owned* representation (e.g. only cloning the subset
of the tree a given read actually needs, rather than the whole project) —
not reaching for `Arc`.

**Open**: whether `queue` has a cap / how `gui-ui` surfaces "an edit is
queued behind a running save" — deferred until there's a real UI to design
it against.

## Why this can't deadlock the UI

Two separate guarantees, one on each side of the channel:

1. **`gui-ui`'s thread never blocks on `gui-core`.** It calls `try_recv_event`
   (non-blocking) once per frame and `send` (also non-blocking — the
   channel is unbounded specifically so a `send` can never block the UI
   thread waiting for the actor to drain it) whenever the user does
   something. If `gui-core`'s actor is wedged — say, `validate()`'s network
   resolution is hung on an unreachable remote with no timeout — `gui-ui`
   simply keeps rendering every frame with that command still marked
   pending; nothing about the UI thread's own progress depends on the actor
   making any.
2. **A hang in one spawned task doesn't stop the actor loop or other spawned
   tasks.** Because slow work runs as its own `tokio::spawn`ed task rather
   than inline in the actor's `select!` arm, a hung task occupies at most
   one of the multi-thread runtime's worker threads. The actor loop itself
   is back at `select!` immediately after spawning, so it keeps accepting
   and handling every command that doesn't need the checked-out state, and
   the runtime's other workers keep servicing any other independent spawned
   work. (This is the concrete reason "actor loop does the work inline" —
   the alternative considered and rejected — was ruled out: it would make
   the actor loop itself the thing that hangs, which stops *all* commands,
   not just ones needing the in-flight state.)

**Known gap this doesn't fully close**: `validate()`'s remote resolution
(`syscalls::RemoteGit`) has no timeout today, at the `syscalls`/`logical`
layer. `gui-core` isolates the *symptom* (the rest of the app stays
responsive) but the underlying spawned task for that one `validate()` call
can still run forever, and per "Exactly one mutation at a time" above,
`mutation_in_flight` stays set — queuing every other command, mutating or
read — until it does resolve or the process exits. A real fix — a
timeout on remote resolution, or a way to cancel an in-flight `validate()`
— belongs in `syscalls`/`logical`, not here; flagging it as a follow-up
rather than solving it by, e.g., racing a local timeout against the spawned
task and abandoning it (that would leave the state permanently checked out,
since the task itself is still running and will eventually try to send a
completion for state the actor thinks it already gave up on).

## Message types

```rust
pub enum Command {
    // Mutating — see "Exactly one mutation at a time".
    LoadProject { path: PathBuf, request: RequestId },
    Validate { request: RequestId },
    Save { request: RequestId },
    /// Save to a given path — a real "Save As" (re-targeting an
    /// already-loaded project), or the *first* save of a `NewProject`
    /// (which has no path at all until one of these succeeds). Same
    /// `SaveError`/`Validated`-precondition as `Save`.
    SaveAs { path: PathBuf, request: RequestId },
    /// A blank project in memory (`logical::draft::create_project`), no
    /// path — purely in-memory, so (unlike `LoadProject`) this is an
    /// inline mutation, not spawned/async; it can't fail, so its
    /// `Outcome::NewProject` carries no `Result`.
    NewProject { name: String, request: RequestId },
    /// Restores the most recent snapshot on the actor's own undo stack —
    /// see "Undo/redo" below.
    Undo { request: RequestId },
    /// The mirror image of `Undo` — see "Undo/redo" below.
    Redo { request: RequestId },
    AddRequirement { module: LogicalPath, name: EntryName, /* ... */ request: RequestId },
    RemoveRequirement { target: LogicalPath, request: RequestId },
    // ... one variant per `logical` operation, plus Load/Validate/Save.

    // Read-only — never blocked by each other, only by a mutation.
    GetEntryDetail { target: LogicalPath, kind: EntryKind, request: RequestId },
    IsRequirementMet { target: LogicalPath, request: RequestId },
    DependencyChain { target: LogicalPath, request: RequestId },

    Shutdown,
}

pub enum Event {
    Completed { request: RequestId, outcome: Outcome },
    /// Pushed whenever the actor's tree changes, whether from a command
    /// gui-ui itself issued or (later) an external change such as a
    /// file-watcher-triggered reload — gui-ui doesn't have to guess.
    TreeChanged(TreeSnapshot),
    ValidationFailed(Vec<logical::ValidationError>),
}
```

`RequestId` (a `u64` counter `gui-ui` increments per command it sends) is how
`gui-ui` matches a later `Event::Completed` back to the UI affordance that
triggered it (e.g. which "saving..." spinner to clear) without needing a
`oneshot` per call — `oneshot::Receiver::try_recv` would work too, but
would mean `gui-ui` juggling a growing set of receivers instead of draining
one `Event` stream, for no real benefit here since gui-ui is already polling
one channel per frame regardless.

### `TreeSnapshot` is a simplified read model, not `logical` data

**Decided**: `gui-ui` never sees a `ProjectDraft`/`ValidatedProject` or any
other `logical` type — `TreeSnapshot` is a purpose-built type living in
`gui-core`'s public API, carrying only what the left-pane tree needs to
render (per node: name, kind, children, and a coarse status — met/unmet/
unvalidated — cheap enough to include on every node without the reads
above). Two reasons this is worth the extra type rather than just handing
over a clone of `ProjectState`: it lets `gui-core`'s internal representation
(the `Draft`/`Validated` alternation, resolved-reference bookkeeping, ...)
change without `gui-ui` recompiling against different `logical` shapes, and
it keeps the amount of data pushed on every `TreeChanged` small — a whole
project's requirement/test/result *text* has no business riding along on a
tree-shape update. Full detail for whatever's selected in the center pane
is a separate, on-demand `GetEntryDetail` read, fetched only for the one
entry currently shown, not folded into the snapshot.

`GetEntryDetail` carries `kind: EntryKind` rather than resolving
`target.name` by trying the requirement/test/result pools in turn: a
requirement, test, and result can share a name within the same module (a
natural pairing — a result named after the requirement it reports on, for
instance), and a fixed-order guess would silently return the wrong entry's
detail whenever that happened. Caught by `gui-ui`'s own `egui_kittest`
interaction tests (a real `sample_project` fixture has exactly this
"definition" naming collision across requirement and result) — see that
crate's README.

`EntryDetail::Requirement` carries `dependencies: Vec<DependencyReferenceKind>`
— `disk`'s own wire type, re-exported here (alongside `LocalGitReference`/
`RemoteGitReference`) the same way `ReferencePath`/`ResultKindV1` already
are, rather than inventing a `gui-core`-specific shape for it. Ordinary
draft data, round-tripped whole through `Command::UpdateRequirement`/
`AddRequirement`'s `RequirementDraft.dependencies` — unlike the local
attachment/template pools, there's no dedicated per-item `Add*`/`Remove*`
`Command` pair for dependencies, since a dependency isn't a file to copy
into place, just a reference; `gui-ui` edits the whole list as plain form
state and submits it on Save, same as `title`/`requirement_text`.

## Undo/redo

Lives entirely in `gui-core`, not `gui-ui`. `gui-ui` never sees a full
`ProjectState` (see `TreeSnapshot`'s own doc above) and has nothing to
snapshot or restore on its own; the actor already owns the one copy of
`ProjectState` that matters, and `ProjectState: Clone` (deliberately —
see "Why not just `Arc<Mutex<ProjectState>>`") makes snapshotting it for
undo purposes free of the `Arc`/`Mutex` rule entirely: a snapshot is just
another independent, owned clone, the same relationship every read
already has to `state`.

The actor carries two bounded stacks —

```rust
struct Actor<F, G> {
    // ...
    undo_stack: VecDeque<ProjectState>,
    redo_stack: Vec<ProjectState>,
}
const UNDO_STACK_CAPACITY: usize = 50;
```

— `undo_stack` capped at `UNDO_STACK_CAPACITY` (a `VecDeque` so the
oldest entry drops from the front once full; not itself a number anyone
asked for, a bounded-memory judgment call, same spirit as `gui-ui`'s zoom
range). `redo_stack` has no separate cap: its size can never exceed
`undo_stack`'s own historical high-water mark (every entry it holds
started out in `undo_stack`, moved there only by `Undo` itself), so a
second explicit cap would just be redundant bookkeeping.

Two new commands, inline like `AddRequirement` and friends — no real
I/O, so no `spawn_blocking`, no `mutation_in_flight`:

```rust
Command::Undo { request: RequestId },
Command::Redo { request: RequestId },
```

`Undo` pops `undo_stack`, pushes the *current* `state` onto `redo_stack`,
swaps the popped snapshot in as the new `state`, fires
`push_tree_changed()`, and completes with `Outcome::Undo(Result<(),
UndoError>)` — `UndoError::NothingToUndo` when the stack is empty
(mirrors `AddChildError`/`UpdateChildError`'s per-function-error-type
convention: `RedoError` is its own type even though it's structurally
identical to `UndoError`, since they're genuinely different operations).
`Redo` is the mirror image.

**When a snapshot is actually pushed**: `mutate_module` (the shared shape
behind every `add_*` command) pushes one unconditionally, even for an
attempt that turns out to fail or no-op (module not found, duplicate
name, ...) — matches that function's own pre-existing "always fires
`TreeChanged`, even on failure, harmless" precedent: undoing to a
snapshot identical to the current state is a no-op, just a slightly
wasted stack slot, not an incorrect one. `remove_from_module`/
`update_in_module`/`add_local_pool`/`remove_local_pool` all *do* have
their own "did this actually change anything" signal already (they only
fire `TreeChanged` when it's true), so they use a more precise two-step
`snapshot_state`/`commit_undo_snapshot` pair instead: clone `state`
*before* the mutation without committing it yet, then only actually push
it once the mutation's own "changed" signal says so — a true no-op
(entry not found, ...) never reaches the stack at all through that path.

**What counts as "a change" — and what doesn't**:

- `LoadProject` (on success) and `NewProject` both *clear*
  `undo_stack`/`redo_stack` outright, not push onto them — undoing across
  a project switch would mean restoring a *previous, unrelated* project's
  state, which isn't what "undo" means to a user who just loaded or
  started something new. A *failed* `LoadProject` leaves both stacks
  alone — whatever was already loaded (if anything) is untouched, so its
  undo history is still valid.
- `Save`/`SaveAs` don't touch the stacks at all — they don't change
  `state`'s *content*, only where (or whether) it's mirrored on disk.
- `Validate`'s own transition is *not* pushed onto `undo_stack` — undo
  tracks edits, not validation state, so the click right after a
  successful `Validate` shouldn't feel like it undid something. Since
  every mutation already calls `ensure_draft()` (demoting `Validated`
  back to `Draft` first) before editing, whichever edit undo *does*
  restore to already carries its own correct Draft/Validated shape
  regardless.

**Availability signaling**: `TreeSnapshot` carries `can_undo`/`can_redo`
booleans (computed from the stacks' emptiness in `push_tree_changed`) —
piggybacked on `Event::TreeChanged`, pushed on every state change
including `Undo`/`Redo`'s own, rather than a separate `Event` variant;
`gui-ui` already re-renders off `TreeChanged`, so no new plumbing was
needed on that side. Its toolbar Undo/Redo buttons are disabled when
these are `false` — same reasoning as the Save/Save As disabled-when-
nothing-to-do fix: clicking into an empty stack would otherwise silently
no-op via `Outcome::Undo(Err(NothingToUndo))`, exactly the class of bug
that fix closed for Save.

**Known gap**: local-pool file operations (`AddRequirementAttachment` and
siblings) go through the same `state`-only snapshot/restore path as
everything else, so an undo correctly reverts the pool's *list entry* —
but if that operation also ever touches a real file outside `state` on
disk (not true as of this writing — these commands only edit the
in-memory `ProjectDraft`/`ValidatedProject`'s own path lists, actual
files are only written at `Save`/`SaveAs` time — but worth restating
explicitly here since it's the kind of assumption that quietly stops
being true), undo would revert the list entry without touching whatever
the file operation did outside `state`, leaving that half unreverted.

## Exit / shutdown

`gui-core` does not implement "graceful shutdown" as a thing `gui-ui` waits
on indefinitely — that would reintroduce exactly the blocking-on-core
hazard this whole design avoids. `gui-core` exposes nothing beyond the
ordinary `Command::Save` (mutating, goes through the normal exclusion
queue) and `Command::Shutdown`; the save-before-exit *prompt*, and the
bound on how long it's willing to wait for that save's `Event::Completed`,
are entirely `gui-ui`'s concern — see that crate's README. `gui-core`'s only
obligation on `Command::Shutdown` is best-effort: finish an in-flight
`save()` if one is already running, don't start a new one, but never assume
it gets to run to completion — the process can and will exit out from under
it regardless of whether `gui-ui` waited first. Concretely, this means
`gui-core` must not rely on `Drop` for anything that matters (e.g. flushing
a write) — `std::process::exit`-style termination from the `gui-ui` side
does not run destructors on other threads.

## Dependencies

`logical`, `disk`, `syscalls` (path deps, same as `cli`), plus `tokio` with
the `rt-multi-thread`, `sync`, and `time` features (no `net`/`fs` —
`syscalls` remains the one crate that touches the filesystem/network
directly, `gui-core` just calls into it from spawned tasks the same way
`cli` calls it synchronously today).

## Testing strategy

**Yes — this crate should be fully unit/branch-testable, no real GUI, no
real IO, no real network, no real clock.** Nothing in `gui-core`'s design
depends on anything a test can't substitute:

- **The actor's dispatch/exclusion logic** (`ProjectState` alternation,
  `mutation_in_flight`, the read/mutation queuing rule) is a plain async
  state machine driven entirely by what arrives on `commands`/`completions`.
  A `#[tokio::test]` can build an actor, hold its own `Sender<Command>`/
  `Receiver<Event>` (i.e. stand in for `CoreHandle` from the test side),
  and drive every branch directly: send two mutating commands back-to-back
  and assert the second's `Event::Completed` doesn't arrive until after the
  first's; send a read while a mutation is checked out and assert it queues
  too; send several reads with none checked out and assert they can
  complete out of order (proving they're not accidentally serialized).
  This is the same shape of test `logical`'s own draft/validate suite
  already uses — synchronous, deterministic, no I/O — just one layer up,
  over channels instead of direct calls.
- **IO and network failure paths reuse `syscalls`'s existing
  fault-injection story.** `syscalls::FaultInjectingFilesystem` (used to
  get `disk` to 100% branch coverage — see that crate's notes) is exactly
  the tool for exercising `save()`'s error branches inside a spawned task;
  a `RemoteGit` fake serves the same purpose for `validate()`'s remote
  resolution paths, including deliberately slow/never-resolving fakes to
  test the "checked out and queuing" state without an actual network hang
  — the fake future can be driven by a `oneshot` the test controls, rather
  than a real timeout, so the test finishes instantly either way.
- **No real time needs to pass.** Anything actor-side that's time-sensitive
  is `tokio::time`-based, so `tokio::time::pause()` plus a test-controlled
  runtime (`#[tokio::test(start_paused = true)]`) makes duration-dependent
  branches deterministic rather than flaky.
- **`cargo +nightly llvm-cov --branch -p gui-core`** should work the same
  way it already does for `disk`/`logical` — same toolchain, same
  `matches!`-over-destructuring discipline to avoid the uncoverable-branch
  trap documented there, applied to this crate's own error/outcome enums.

**What this doesn't cover**: `gui-ui`'s own behavior — `gui-core` can be
tested completely in isolation from any actual window or render loop, but
that only proves the message-passing contract holds, not that a button
click actually produces the right `Command`. That's `gui-ui`'s own testing
section's job.

## Open questions

- Exact `Command`/`Event` variant list — sketched above, not exhaustive;
  should track `logical`'s operations catalog one-for-one as that catalog
  gets implemented.
- `queue` cap / how `gui-ui` surfaces "an edit is queued behind a running
  save" — noted under "Exactly one mutation at a time," deferred until
  there's a UI to validate it against.
