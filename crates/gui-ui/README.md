# gui-ui

**Status**: `gui-config.ron` loading, the exit-prompt state machine, the
layout, and the four distinct per-kind center-pane forms (`src/forms.rs`)
are all implemented — and each form now does **both** creation and
editing. Toolbar "New Requirement/Test/Result/Module" opens a blank form
(Create sends an `Add*` command); clicking an existing requirement/test/
result in the tree opens the *same* form pre-filled from its
`GetEntryDetail` reply, in edit mode (Save sends the matching `Update*`
command instead, name field disabled — renaming isn't supported). Either
way, a failure reports inline and the form stays open; success closes a
creation form, but leaves an edit in place showing what was just saved.
Modules have no *edit-mode form* (nothing of their own to edit beyond
name/children, which the tree already shows) — but their name specifically
is renameable, via a dedicated dialog (see below), not this form.

A toolbar "Attachments…" button opens a modal for the current module,
listing its attachment/template pools (fetched via `GetModulePools`) with
per-item Remove buttons and an Add field for each pool. A successful add/
remove marks the project dirty and re-fetches the list; a failure reports
inline. "Current module" is now a real, independently-tracked concept
(`selected_module`) rather than inferred from the leaf selection: every
tree module node has its own selector button (◉/○, next to its
`CollapsingHeader`, which still only toggles expand/collapse) alongside
the project root's, and `selected_module` stays in sync whenever a leaf is
clicked too. Fixing this also fixed a real bug: the tree's root node had
been pushing the *project's own display name* into child paths as if it
were a real module-path segment, which would have sent every root-level
`Command` with the wrong `module`/`LogicalPath` in the actual running
app — invisible until now since rendering isn't unit-tested (see Testing
strategy) and the bug was in `view.rs`'s path-building, not the
(correctly-tested) logic layer.

Each of the three editable forms (Requirement/Test/Result) also has a
"Local attachments" section (Test gets a second, separate "Local template
files" section) shown only in edit mode — a local pool belongs to an
*existing* entry, so there's nothing to attach to during creation. Add/
remove there hits `gui-core`'s 8 local-pool commands directly and applies
the result to the form's own `attachments`/`template_files` list in place
(`apply_local_pool_change`), rather than re-fetching `EntryDetail` the way
the module-level Attachments dialog re-fetches `ModulePools` — a full
re-fetch here would rebuild the whole form via `apply_entry_detail` and
discard any unsaved edits to its other fields (title, text, ...) in the
meantime. `local_pool_ops`, keyed by `RequestId`, is what makes that
possible: it remembers which form/field/path a pending add or remove was
for, since the `Outcome` itself doesn't carry the path back.

Each module tree node also has a rename button (✎, next to its ◉/○
selector) opening a small "Rename Module" modal — new name, Rename/Cancel,
inline error on failure. A successful rename updates `selected_module` in
place if the renamed module was (or contained, for a nested current
module) the current one, so "current module" doesn't keep pointing at a
name that no longer exists after its own rename.

The Result form's "Requirement path"/"Test path" fields each gained a
"Pick…" combo box alongside the existing free-text field — populated by
walking `self.tree` for every `Requirement`/`Test` node (`view.rs`'s
`flatten_leaf_paths`, purely client-side, no new `gui-core` read needed)
and formatting the selected one as the exact
`/[modules/<sub>/]*requirements|tests/<name>` string
`logical::path::parse_reference_path` expects
(`absolute_reference_path`) — so the user no longer has to know that
syntax by heart, without losing the ability to type/paste a path by hand.
These two functions are pure and genuinely unit-tested (`view::test`), the
first tests directly in `view.rs` rather than `lib.rs` — the Testing
strategy's "keep logic out of rendering" principle applied to a rendering
*helper* rather than a `GuiApp` method. **Commits are still typed by
hand** — auto-filling a picked target's current commit would need
`EntryDetail` to carry it (an easy addition, `RequirementDraft`/
`TestDraft` already have the field) plus a second `GetEntryDetail`
round-trip and its own stale-reply tracking, which felt like a
disproportionate amount of new plumbing for what's still a materially
better UX without it; left as the one open item from this pass.

The File menu now has an "Open Recent" submenu, backed by a second small
settings file (`recent.ron`, next to `gui-config.ron`) recording every
project a `LoadProject`/`SaveAs`/`Save` actually succeeded against — see
"Recently opened projects" below. New Project, Open Project, and an
"Open Recent" click are all gated on unsaved changes now too — a
confirmation prompt (Continue/Cancel) interrupts any of the three when
`self.dirty`, rather than silently discarding whatever hasn't been saved
— see "Unsaved-changes confirmation" below.

## What this crate is for

The single-threaded, synchronous presentation layer. `gui-ui` owns the
window, renders every frame, and reacts to user input — nothing it does is
`async`, and it never touches `syscalls`, `disk`, or `logical` directly. All
project state and every mutating operation goes through `gui-core`'s
[`CoreHandle`](../gui-core/README.md#threading-model) over non-blocking
channels. This split is the whole point: `gui-ui` can promise "the window
always redraws, the Exit button always works" only because it has made
itself incapable of blocking on anything that might hang.

## Toolkit: egui / eframe

Immediate-mode, single-threaded by construction (an `eframe::App::update`
call *is* one frame — there's no separate retained widget tree to keep in
sync with `gui-core`'s state), and eframe's `run_native` already gives us
the "one thread runs the whole UI" structure this design wants, rather than
something we'd have to impose on top of a toolkit that expects to manage its
own threads.

```rust
struct GuiApp {
    core: gui_core::CoreHandle,
    // gui-ui's own copy of what it needs to render — updated from
    // Event::TreeChanged, never read live from gui-core.
    tree: TreeSnapshot,
    selection: Option<LogicalPath>,
    editor: EditorState,
    pending: HashMap<RequestId, PendingKind>,
    status: StatusLine,
    /// Set on every mutating Command gui-ui sends, cleared on a matching
    /// Event::Completed for a Save. gui-ui's own bookkeeping — it does not
    /// ask gui-core whether there are unsaved changes. See "Exit".
    dirty: bool,
    exit_dialog: Option<ExitDialogState>,
    config: GuiConfig,
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        while let Some(event) = self.core.try_recv_event() {
            self.apply(event); // update self.tree/self.pending/self.status
        }
        // ... render menu bar / toolbar / left pane / center pane / status bar
        ctx.request_repaint(); // keep polling even with no input, so a
                                // completed Event shows up promptly
    }
}
```

`self.tree`/`self.selection`/`self.editor` are `gui-ui`'s own state, not a
borrow of anything in `gui-core` — there is no shared state, only a
snapshot `gui-ui` was handed and will replace wholesale the next time an
`Event::TreeChanged` arrives. Editing the center pane (see below) mutates a
local draft buffer; committing it sends a `Command`, it does not mutate
`self.tree` directly.

## Layout

Fixed regions, standard desktop-app arrangement, built with egui's own
`TopBottomPanel`/`SidePanel`/`CentralPanel`:

```
+----------------------------------------------------------+
| Menu bar (File, Edit, View, ...)                          |
+----------------------------------------------------------+
| Toolbar (New, Save, Validate, ...)                         |
+---------------+--------------------------------------------+
| Left pane      | Center pane                                |
| (tree view:    | (editor for the selected element, or a     |
| modules, each  | "create new ___" form when nothing valid   |
| with its own   | is selected / "new" was chosen from the    |
| requirements/  | toolbar)                                   |
| tests/results  |                                             |
| folders)       |                                              |
|               |                                              |
+---------------+--------------------------------------------+
| Status bar (current project path, pending-operation spinner,|
| last validation result)                                     |
+----------------------------------------------------------+
```

- **Menu bar**: `egui::menu::bar` inside a top `TopBottomPanel`. File has
  New Project…/Open Project…/Open Recent/Save/Save As…/Exit; Edit/View
  are still deliberately absent (see below). Entries dispatch the same
  `Command`s the toolbar does — no separate code path. Open Project… and
  Save As… pop a real native OS folder picker (the `rfd` crate,
  `rfd::FileDialog::new().pick_folder()`) — a project is a directory, not
  a single file, so it's always the folder-picker form, never a file
  picker. This is a deliberate, narrow exception to "never block the
  render thread" (see that section below): the call blocks the calling
  thread until the user dismisses the *OS's own* modal dialog, but that's
  bounded by the user's own interaction with a standard, familiar system
  dialog — a fundamentally different kind of "block" than `gui-core`
  hanging unpredictably, which is what that rule actually guards against.
  New Project… asks for a name instead — a project's name isn't a
  filesystem path, so there's nothing for a folder picker to supply — via
  a plain modal text field, `render_new_project_dialog`. "Open Recent" is
  a submenu (only rendered when it has anything in it) listing
  `self.recent.paths` — see "Recently opened projects" below.
  Save/Save As… are both disabled (`self.tree.is_some()`) whenever
  nothing is loaded — a click would otherwise only ever come back
  `Outcome::NoProjectLoaded`, which nothing in `gui-ui` surfaces anywhere,
  so it would silently do nothing rather than being visibly unavailable.
  Save also has a second, narrower gap it closes on its own: a project
  can be loaded (so enabled) but not yet have a known on-disk path (a
  `NewProject` never saved before) — clicking it in that state falls back
  to the same native picker Save As… uses (`GuiApp::save_button_clicked`,
  driven by `GuiApp::needs_path_before_saving`) rather than sending a
  `Command::Save` that would fail the exact same silent way. New
  Project…/Open Project…/an "Open Recent" entry are all additionally
  gated on unsaved changes — see "Unsaved-changes confirmation" below.
- **Toolbar**: a second `TopBottomPanel` below the menu bar, icon buttons
  for the highest-frequency operations (New Requirement/Test/Result, Save,
  Validate, Undo/Redo, Back/Forward) plus the Exit button. **Exit is not
  "just another toolbar button that sends a Command"** — see "Exit"
  below, it's handled specially. `ui.horizontal_wrapped`, not
  `ui.horizontal` — with Undo/Redo/Back/Forward added on top of the
  original buttons, a plain `horizontal` silently overflows `egui`'s own
  merely-800px-wide default window: the row just kept extending past the
  visible edge rather than wrapping, pushing "Attachments…" half
  off-screen where clicks land nowhere (found by a real interaction test
  failing after these buttons were added, not by inspection — see
  Testing strategy's non-obvious-findings list). Back/Forward and the
  four "New ___" buttons are gated on unsaved *form* edits — see
  "Unsaved-changes confirmation (form-level)" below.
- **Left pane**: `egui::Panel::left`, a tree (`egui::CollapsingHeader` per
  module, nested) built from `self.tree`. Resizable by dragging its right
  edge, over an explicit `120.0..=900.0` range (`.size_range(...)` in
  `render_left_pane`) — wide enough for a deeply-nested module/
  requirements/tests/results tree without either pane getting crushed,
  still bounded rather than egui's own default of unbounded-max. Its
  inner `egui::ScrollArea` is `.auto_shrink([false, false])` — without
  that, the scroll area shrinks to fit its content's natural width every
  frame (egui's default), which fights the drag: the panel reports the
  width just dragged to, the auto-shrunk content immediately reports a
  narrower natural width back, and the resize barely moves at all instead
  of holding wherever it's dragged to (found from a real report of
  exactly that symptom, not caught by any test — nothing in
  `tests/interaction.rs` simulates a resize drag). Clicking a leaf (or a
  module's own ◉/○ selector) sets `self.selection`/`selected_module` and
  loads that entry into the center pane's editor state; it does not
  itself send any `Command` — gated on unsaved form edits first, same as
  the toolbar's Back/Forward/"New ___" buttons (see "Unsaved-changes
  confirmation (form-level)" below). Within a
  module, leaves
  are grouped under three collapsible "requirements"/"tests"/"results"
  folders (`view.rs`'s `render_module_children`/`render_leaf_group`) —
  mirroring `disk`'s own on-disk layout (each module really does have
  separate `requirements/`, `tests/`, `results/` directories), not just a
  display convenience. A folder is omitted entirely when a module has
  none of that kind (an empty new module shows no folders at all); the
  three that do exist default to expanded, so a leaf underneath is
  immediately clickable, not hidden behind an extra expand click.
  Above the tree, a filter bar (`self.tree_filter`, a plain `TextEdit`
  plus a "×" clear button) narrows the tree to leaves whose fully
  qualified logical path (e.g. `/requirements/definition`,
  `/modules/setup/tests/generic_test` — the same form
  `absolute_reference_path` builds for the Result form's path pickers)
  contains the filter text, case-insensitively; an empty filter shows
  everything. `node_matches_filter` (`view.rs`) recurses top-down: a
  module matches only when some descendant leaf matches, and a
  non-matching module is skipped before any of its children are drawn
  at all — so filtering hides whole empty branches, not just leaves.
  The filter field's own width is bounded
  (`.desired_width(150.0)`) — left unbounded, it inflates the left
  pane's measured natural width and can push center-pane widgets (a
  `ComboBox` trigger, in one real case) off the right edge of a narrow
  window, same overflow-past-viewport class of bug as the toolbar's
  own `horizontal_wrapped` fix above (again found via a real
  interaction test failing, not by inspection).
- **Center pane**: `CentralPanel`, keyed off `self.selection`. Selecting an
  existing requirement/test/result shows its read-only viewer by default
  (an "Edit" button switches to the editable form for the same entry —
  see "Center pane: distinct forms per kind" below); selecting a module,
  or picking "new ___" from the toolbar, goes straight to a creation
  form (nothing to view yet). Edits live in local `self.editor` state
  until an explicit Save/Apply action sends the corresponding
  `Command::AddRequirement`/whatever — no autosave-as-you-type, so a
  half-finished edit is never sent to `gui-core` and never risks racing
  another in-flight command.
- **Status bar**: `TopBottomPanel::bottom`, reads `self.status` — project
  path, a spinner while `self.pending` is non-empty, and the outcome of the
  last `Command::Validate` (`Event::ValidationFailed`'s error count, or a
  green "valid" state after `Event::Completed` for a validate request with
  no errors). Its dirty indicator is three-way, checked in this order:
  "No project loaded" when `self.tree` is `None`, then "● unsaved
  changes"/"saved" off `self.dirty` once a project actually exists —
  `self.dirty` alone used to drive this (defaulting `false`), which
  meant a fresh launch with nothing open said "saved", misleadingly
  implying a project existed and had been saved. Its far right corner
  (`ui.with_layout(Layout::right_to_left
  (Align::Center), ...)`) holds the zoom controls, left to right: Reset,
  `−`, an editable value field, `%`, `+`. `+`/`−` step 10 percentage
  points per click, clamped to `80..=400` (`ZOOM_MIN_PERCENT`/
  `ZOOM_MAX_PERCENT`/`ZOOM_STEP_PERCENT` in `lib.rs`); Reset jumps
  straight to `100` (`ZOOM_DEFAULT_PERCENT`) — `egui`'s own unzoomed
  `zoom_factor` of `1.0`. The value field itself
  (`egui::TextEdit::singleline(&mut self.zoom_input)`) takes free-form
  typed input, validated only once focus leaves it
  (`response.lost_focus()`, which a singleline field's own Enter
  handling also triggers, not just clicking away — `GuiApp::
  zoom_input_submitted`): a valid number is parsed and clamped to the
  same `80..=400` range every other control uses; invalid text (empty, a
  parse failure) is silently rejected, and either way the field is
  resynced to whatever the real, possibly-clamped value ends up being
  (`GuiApp::sync_zoom_input`) rather than left showing the rejected or
  out-of-range text. Every one of these four ways to change the zoom
  level funnels through one `GuiApp::set_zoom_percent(percent)` — clamp,
  resync the field, persist — so there's exactly one place that logic
  lives. Applied every frame via `ui.ctx().set_zoom_factor(self.config.
  zoom_percent as f32 / 100.0)` in `GuiApp::ui` (a no-op when unchanged,
  so unconditional is fine), and the new level is written straight back
  to `gui-config.ron` on every change (`GuiConfig::save`, see
  "Configuration" below) so it survives a restart.

## Never block the render thread

Everything in `update()` must return within a frame budget regardless of
`gui-core`'s state. Concretely, that means:

- `CoreHandle::try_recv_event` and `CoreHandle::send` are the *only* two
  calls `gui-ui` makes across the thread boundary, and both are documented
  (in `gui-core`) as non-blocking/non-async. `gui-ui` never calls anything
  that awaits, never does a blocking channel `recv`, never does its own
  filesystem/network IO.
- A `Command` that's still outstanding is tracked in `self.pending` (keyed
  by the `RequestId` `send` was given), and the relevant UI affordance
  (Save button, the field being edited, ...) reflects that — disabled,
  spinner, whatever's appropriate — until the matching `Event::Completed`
  arrives. There is no synchronous "did it work" return value to wait on;
  every mutating action in this UI is fire-and-poll, not fire-and-forget-
  and-block or fire-and-immediately-know.
- If `gui-core` never responds (the known gap noted in its README — an
  unbounded remote-resolution hang during `validate()`), the affected
  `self.pending` entry simply never clears. The rest of the UI keeps
  working — other panes, other fields, even other in-flight requests that
  don't depend on the checked-out state — because nothing about rendering
  them depends on that one pending entry resolving.
- **The one deliberate exception**: Open Project…/Save As… call
  `rfd::FileDialog::pick_folder()` directly in their click handler, which
  *does* block the calling (render) thread until the dialog closes. This
  is still consistent with the rule above in spirit, not a hole in it:
  every case this section actually guards against is `gui-core` — a
  background actor whose completion time is entirely outside `gui-ui`'s
  control and can be unbounded (the `validate()` gap just above is
  exactly that). A native OS folder picker is the opposite: it's `gui-ui`
  itself presenting a standard, familiar, user-dismissable modal at the
  moment the user asked for one, no different in kind from any other
  modal dialog in this app (the exit-confirmation dialog, the rename-
  module dialog, ...) except that the OS draws it instead of egui. It
  never touches `gui-core` or the `CoreHandle` channel at all.

## Exit

The Exit button (and the window's own close control, and File → Exit) never
send `Command::Shutdown` and block waiting for anything back — that would
reintroduce the exact hang this whole design exists to avoid. Instead, exit
happens in up to two stages: an optional save-prompt dialog (only when
there are unsaved changes), then an unconditional immediate close that
nothing can prevent.

### Stage 1: prompt to save, bounded so it cannot hang

If `self.dirty` is `false`, skip straight to Stage 2. Otherwise, the Exit
handler opens a modal `self.exit_dialog = Some(ExitDialogState::Asking)`
and renders a dialog with three choices, drawn every frame like any other
egui widget — this is ordinary local UI state, not a blocking call, so it
costs nothing and cannot hang by construction:

```rust
enum ExitDialogState {
    Asking,
    Saving { request: RequestId, deadline: Instant },
    TimedOut { request: RequestId },
}
```

- **Discard** → drop `exit_dialog`, go straight to Stage 2.
- **Cancel** → drop `exit_dialog`, exit is aborted, back to normal use.
- **Save** → send `Command::Save` (the same mutating command the Save
  toolbar button sends — no special "exit save" variant needed), and switch
  to `Saving { request, deadline: Instant::now() + self.config
  .save_on_exit_timeout }` (see "Configuration" below — defaults to 15s).
  Every frame while in this state:
  - if `Event::Completed { request, .. }` for that request has arrived
    (via the normal `try_recv_event` polling loop, same as any other
    command), the save succeeded (or failed — either way it's no longer
    pending): go to Stage 2.
  - else if `Instant::now() >= deadline`, switch to `TimedOut { request }`
    and show "Still saving — Exit anyway and lose unsaved changes, or keep
    waiting?" with two buttons (Exit anyway → Stage 2; Keep waiting → reset
    the deadline and stay in `Saving`).

**This is the "cannot easily hang" property, concretely**: the wait for
`Save`'s completion is never an unbounded blocking call — it's the same
non-blocking per-frame poll every other pending command already uses,
capped by a wall-clock deadline the UI thread itself measures, so the UI
stays fully responsive (rendering, other buttons, even Cancel) the entire
time, and the *worst case* is a bounded number of seconds before the user
is explicitly handed a choice rather than the app quietly wedging on Exit
forever. (Compare: a design that did `core.blocking_recv()` here would
hang the whole window, including the ability to click "Exit anyway,"
exactly if `gui-core`'s save task is the thing that's wedged — which is
precisely the scenario this needs to survive.)

### Stage 2: unconditional immediate close

1. Sends `Command::Shutdown` on `self.core` — best-effort, fire-and-forget,
   same non-blocking `send` as every other command. Gives `gui-core` a
   chance to stop starting new work, per its README, but nothing here
   depends on it succeeding or being observed.
2. Immediately requests the window close (`ctx.send_viewport_cmd(egui::
   ViewportCommand::Close)`), which ends `eframe::run_native`'s loop and
   returns control to `main()`.
3. `main()` returns. Process exit tears down every thread, including
   whatever `gui-core`'s tokio runtime workers are doing, whether or not
   they'd finished — this is *why* the exit is guaranteed to be prompt even
   if `gui-core` is wedged: nothing in this stage waits on it, the OS
   reclaims the hung worker thread along with everything else at process
   teardown.

**Why not just "wait up to Nms for shutdown, then force it" instead of a
whole save-prompt dialog?** That was considered and rejected as the *whole*
design: it either saves silently (risking writing a half-finished edit the
user never asked to persist) or discards silently (risking data loss the
user was never told about) — neither tells the user what happened. Making
the save explicit and the wait visible costs one modal dialog in the one
case that needs it (unsaved changes) and adds nothing to the common case
(nothing dirty → straight to Stage 2, same latency as before).

## Configuration: `gui-config.ron`

`gui-ui`-level settings — `save_on_exit_timeout` and `zoom_percent` — live
in a `gui-config.ron` file, loaded once at startup before `GuiApp` is
constructed:

```rust
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct GuiConfig {
    save_on_exit_timeout: Duration,
    zoom_percent: u32,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig { save_on_exit_timeout: Duration::from_secs(15), zoom_percent: 100 }
    }
}
```

```ron
GuiConfig(
    save_on_exit_timeout: Duration(secs: 15, nanos: 0),
    zoom_percent: 100,
)
```

Unlike `save_on_exit_timeout` (hand-edit only — nothing in the running app
ever changes it), `zoom_percent` is also written *back* to this same file
by `GuiConfig::save`, every time the status bar's zoom `+`/`−` is clicked
(`GuiApp::persist_config`) — the last-used zoom level survives a restart.
`save` serializes through the same `ron::Options` (`Extensions::all()`,
see below) `load` parses with, not the bare `ron::ser::to_string_pretty`
free function — the free function omits the struct name RON normally
makes optional, but `Extensions::all()` includes `EXPLICIT_STRUCT_NAMES`,
which makes `load` *require* it, so serializing without matching options
produced a file `load` itself couldn't parse back
(`ExpectedStructName`) — caught by `config.rs`'s own
`save_then_load_round_trips_every_field` test, not by hand. Also unlike
`save_on_exit_timeout`, this is `gui-ui` doing its own synchronous
filesystem *write*, not just the read `load` already does — same narrow,
deliberate exception to "never do its own filesystem IO" (see "Never
block the render thread" above): this is `gui-ui`'s own local settings
file, entirely outside `gui-core`'s project-data path, and the write is
tiny, bounded, and only ever happens in direct response to a click, never
per-frame.

**Decided: every RON extension enabled** (`ron::extensions::Extensions::
all()`, via `ron::Options::default().with_default_extension(Extensions::
all())`) — deliberately *not* `disk`'s narrower, hand-picked set (see
`disk/src/util.rs`: `EXPLICIT_STRUCT_NAMES | IMPLICIT_SOME |
UNWRAP_NEWTYPES | UNWRAP_VARIANT_NEWTYPES`). `disk`'s extension choice is
scoped to what a hand-authored *project* file should look and round-trip
like; `gui-config.ron` is a separate, much smaller, gui-ui-only settings
file with no round-trip/authoring-consistency concerns riding on it, so
there's no reason to withhold any convenience RON offers here — worst case
an unused extension does nothing, since this format has no other tooling
depending on a specific written shape the way project files do.

A missing file, or a missing/unknown field inside it, is not an error —
`#[serde(default)]` at the struct level means every field not present
falls back to `Default::default()`, so either field alone can be
hand-overridden without repeating the other.
Malformed RON (a parse error) *is* still surfaced — to the status bar as a
warning, not a startup failure — since the app should still start with
defaults rather than refuse to run over a typo in an optional file.

`gui-ui` reads this file itself with a direct `ron`/`serde` dependency,
rather than going through `syscalls`/`disk` — app configuration isn't part
of the project data model those crates own, and pulling either in only for
this one file read would be a heavier dependency than the read warrants
(see "Dependencies" below: `gui-ui` otherwise depends on neither).

**Open**: exact config file location (platform config dir vs. next to the
executable vs. project-adjacent) — not decided; the field/shape above is
settled regardless of where the file ultimately lives.

## Recently opened projects: `recent.ron`

A second, separate small file (`recent.rs`), same load/save/RON-options
shape as `GuiConfig` (see above) but its own type — `RecentProjects {
paths: Vec<PathBuf> }`, most-recent-first, deduplicated (`record`'s own
job: a path already present moves to the front rather than growing a
second entry), capped at `MAX_ENTRIES` (10) so the "Open Recent" submenu
stays a glance-able list, not something to scroll through. Loaded once at
startup next to `GuiConfig` (`main.rs`, a sibling `recent.ron` next to
`gui-config.ron`), passed into `GuiApp::new` the same way.

**Recorded on every successful `LoadProject`/`SaveAs`/`Save`** — not just
the first time a project's ever opened. `GuiApp::record_recent_project`
is the one chokepoint: `apply_project_path_result` (the `LoadProject`/
`SaveAs` completion handler) calls it once a path is confirmed, and the
plain `Outcome::Save(Ok(()))` arm calls it too (against `self.project_path`,
which Save never changes — it's already known, Save is only enabled once
it is) so a project you keep working in and saving stays at the top of
the list rather than sinking as you open other things. Same best-effort,
no-visible-failure-surface persistence precedent as `persist_config`
(`GuiConfig::save`) — a failed write here doesn't block or crash
anything, the in-memory list (and thus this session's own "Open Recent"
menu) still updates either way, it just might not survive a restart.

**Clicking an entry loads it** (`GuiApp::open_project`, the same method
Open Project's own native-picker result goes through) — gated on unsaved
changes exactly like New Project/Open Project, see below.

## Unsaved-changes confirmation (project-level)

New Project and Open Project (via the native picker or an "Open Recent"
click) all discard `self.dirty` content wholesale if there's any —
unlike Save/Save As, which *resolve* dirty state, these three *replace*
it outright with a different project's (or a blank one's). Each checks
`self.dirty` (a plain field read, same as the status bar's own dirty
check) before proceeding, and shows a confirmation prompt instead when
it's `true`:

```rust
enum PendingProjectAction {
    NewProject,
    OpenProject,
    OpenRecent(PathBuf),
}
```

`unsaved_changes_dialog: Option<PendingProjectAction>` remembers which
action to resume if the user clicks Continue; Cancel just clears it,
leaving everything untouched. `unsaved_changes_confirmed` resumes
`NewProject` (opens the name-entry dialog, exactly what a direct click
would have done) and `OpenRecent` (calls `open_project` directly) itself;
`OpenProject` hands back to `render_unsaved_changes_dialog` instead,
since popping the native folder picker (`rfd::FileDialog`) is view-layer
— the same split `save_button_clicked`'s own picker-vs-`Command` fallback
already follows.

**Deliberately not a save-first option** — just Continue (discard) or
Cancel, not a three-way "Save, Discard, Cancel" the way the exit prompt
offers (see "Exit" above). The exit prompt's extra complexity (a bounded
wait for an in-flight save, a timeout) exists because exiting is
one-directional — there's no "keep working" to fall back to if the save
hangs. Here, Cancel already *is* "keep working": the user can just click
Save themselves first, then retry, so a built-in save-and-continue path
wasn't judged worth the same state-machine complexity for a first pass.

## Unsaved-changes confirmation (form-level)

A second, separate confirmation, easy to conflate with the one above but
guarding a genuinely different kind of unsaved state: `self.dirty`
(above) means a mutation already reached `gui-core` but hasn't been
*saved to disk* yet; this one means a requirement/test/result form has
typed field edits that haven't even been *submitted* to `gui-core` —
losing them isn't a Save away, it's gone the moment `self.editor` gets
replaced. Every navigation that would silently do that — a different
tree leaf, a different module, Back/Forward, or any of the toolbar's
"New ___" buttons — now checks first and prompts instead:

```rust
enum PendingNavigation {
    Select { target: LogicalPath, kind: EntryKind },
    SelectModule(Vec<EntryName>),
    Back,
    Forward,
    NewRequirement,
    NewTest,
    NewResult,
    NewModule,
}
```

**Tracking "has anything actually changed"**: `RequirementFormState`/
`TestFormState`/`ResultFormState` each carry `edited: bool` (not
`ModuleFormState` — it has no view/edit split to protect in the first
place, see "Center pane" above). Set from each editable widget's own
`Response::changed()` (`ui.text_edit_singleline(&mut form.title).changed()`,
etc.) — a one-line wrap at each existing widget call, not a stored
pristine snapshot diffed every frame; a design question raised and
settled explicitly before implementing (see "Considered and rejected"
under "Follow-up: Save/Cancel moved next to the heading" above, which
covers the same "is field-level dirty-tracking worth it" trade-off —
concluded worth it *here*, unlike gating Save's own visibility, because
here it's preventing real data loss, not just deciding what to show).
Composing the "Add dependency" entry doesn't set it (nothing real has
changed until "Add dependency" is actually clicked); removing/adding/
editing an *existing* dependency does. Reset `false` in
`apply_entry_detail` (a freshly (re)loaded form starts pristine) and on
a successful `Update*` (`apply_update_result` — saved values now match
displayed ones); a *failed* save leaves it `true`, since the content is
still genuinely unsaved. Local attachments/templates don't participate —
those submit their own `Command` immediately on Add/Remove (already
covered by `self.dirty` above), not deferred to this form's own Save.

**Deliberately excluded: the form's own Cancel button.** Cancel already
means "discard this edit," explicitly and unambiguously — prompting
"are you sure you want to discard" after a click that already says
exactly that would be pure confirmation fatigue, not a safety net. Every
`view.rs` click handler this gate touches reads `self.editor_has_unsaved_edits()`
(a plain field read) and either proceeds directly or calls
`unsaved_form_dialog_opened(PendingNavigation::...)`; `unsaved_form_dialog_confirmed`
resumes the real underlying method (`select`/`back_clicked`/
`new_requirement_clicked`/...) directly — unlike the project-level
prompt's `OpenProject` case, nothing here needs a native picker, so there's
no view-layer handback required.

## Dependencies

`gui-core` (path dep, for `CoreHandle`/`Command`/`Event`/`TreeSnapshot`),
`eframe`/`egui`, plus `ron`/`serde` (workspace deps, already used by
`disk`/`logical`) for `gui-config.ron`. No `tokio` dependency — `gui-ui`
links no async runtime at all.

## Center pane: distinct forms per kind

**Decided**: four distinct forms (requirement/test/result/module), not one
generic "new entry of kind X" form — each `logical` `add_*` operation's
fields differ enough that a generic form would mostly be a big match
statement in disguise, both for the "create" and "edit" cases. Selecting a
tree node or choosing "new ___" from the toolbar picks which form
`self.editor` holds; there's no shared "entry" abstraction in the UI layer
beyond `self.selection: Option<LogicalPath>` pointing at what's loaded.

**Viewer by default, edit on request**: for an *existing* requirement/
test/result (a create-mode form is always editable — there's nothing to
view yet), selecting it in the tree lands on a read-only viewer, not the
editable form — every field renders as a plain `ui.label`, no Save/
Cancel, just an "Edit" button next to the heading. Clicking it switches
to the exact same form, now editable, with Save/Cancel back — pinned in
that *same* spot, next to the heading, rather than at the bottom below
every field (see "Requirement dependencies" below on why that matters
for a form that can run long).
`forms.rs`'s three viewable form states (`RequirementFormState`/
`TestFormState`/`ResultFormState` — not `ModuleFormState`, see below)
carry this as `read_only: bool`, alongside the existing `editing_target:
Option<LogicalPath>` that already distinguished create from edit — so a
form now has three logical modes (`editing_target: None` = create;
`Some(_)` with `read_only: true` = view; `Some(_)` with `read_only:
false` = edit), not two. `GuiApp::apply_entry_detail` sets `read_only`
from `current_nav_mode()` (see "Forwards/backwards navigation" below);
`view.rs`'s three `render_*_form` functions branch on it field-by-field
rather than duplicating the whole function, so the read-only and
editable renderings can't drift out of sync on which fields exist.
Local attachment/template lists follow the same split: the viewer shows
the list, just without the Remove buttons or the "add a new one" row.

Modules are the one kind left out: they have no `EntryDetail` of their
own to view (see that type's own doc comment — a module's "content" is
just the children the tree already shows) and no edit form beyond the
separate Rename Module modal, so `render_module_form` is unchanged,
create-only, no viewer/read-only split.

### Requirement dependencies: viewed, added, removed, edited

A requirement's `dependency`/`dependencies` field (`gui_core::
DependencyReferenceKind` — `RequirementReferenceV1`/`RemoteReferenceV1`/
the bare `Submodules`, see `disk`'s README) is editable in the
requirement form, alongside title/text/guidance. `forms.rs`'s
`DependencyDraft` is the plain-`String`-fields shape the picker/text
fields need — `LocalRequirement { path, commit }`/`Remote { url, path,
commit }`/`Submodules`, converted to/from the wire type only at the
form's edges (`from_core`/`to_core`; `Remote`'s optional `path` collapses
to an empty `String` and back, trimmed before checking). `Display` is
the viewer's own one-line summary (`"{path} @ {commit}"`, etc.).

Unlike attachments, this is **not** a local-pool round trip — no
per-item `Command`, no `RequestId` tracking. A dependency is a reference,
not a file to copy into place, so `RequirementFormState.dependencies:
Vec<DependencyDraft>` is edited as plain in-memory form state (push/
remove/field-edit, all direct mutation inside `render_requirement_form`,
same as `form.title`/`form.result_kind`) and submitted whole via
`build_command` on Save/Create — the exact same shape every other draft
field already follows. `view.rs`'s `render_dependency_kind_picker`/
`render_dependency_fields` are shared between an existing dependency's
own row (with a "Remove" button, three radio buttons to switch its
variant) and the "Add dependency" composer below the list
(`form.new_dependency`, pushed onto `dependencies` and reset on click) —
factored out specifically so the two don't drift apart on which fields
each variant needs. The dependencies section isn't gated on `editing`
the way attachments are: a brand-new, never-saved requirement can have
dependencies typed in before it's ever created, since nothing about them
needs the entry to already exist on disk.

**Found while adding this**: the center pane (`render_center_pane`) had
no `ScrollArea` at all — a short form never needed one, but the
dependencies section plus the "Add dependency" composer is enough extra
height that a real requirement's Save/Cancel row can land below the
bottom edge of a modest window, unreachable, no visible sign anything's
missing. Same overflow-past-viewport bug class as the toolbar/zoom/
filter-field fixes documented in Testing strategy below — fixed by
wrapping the whole `CentralPanel` body in `egui::ScrollArea::vertical()
.auto_shrink([false, false])`, covering every form, not just
Requirement's. A second, `egui_kittest`-specific finding came out of
testing this: `Node::click()` sends a synthetic `PointerButton` event at
`rect().center()`, which silently does nothing for a node scrolled out
of the *test* window's fixed-size viewport (`scroll_to_me()`'s AccessKit
`ScrollIntoView` action didn't move it either, confirmed empirically) —
`Node::click_accesskit()` (an AccessKit `Action::Click` dispatched
straight at the node by id, sidestepping on-screen position entirely) is
what several of this file's tests now use for a button that might be
below the fold, rather than fighting to scroll it into view first.

**Follow-up: Save/Cancel moved next to the heading.** The `ScrollArea`
fix above makes a long form's Save reachable, but still only by
scrolling to find it — easy to miss on a first look at a form with
several dependencies and local attachments. Save/Cancel (and, in read-
only mode, "Edit") now render in the same `ui.horizontal` as the heading
itself, at the very top of every one of the three viewable forms — always
visible without scrolling, regardless of how long the rest of the form
runs, while the *fields themselves* still need Save clicked to actually
apply (no autosave-as-you-type, unchanged — see "Center pane" above).
The bottom-of-form Save/Cancel row this replaced is gone entirely, not
duplicated; `saving_an_edit_to_an_existing_requirement_keeps_the_form_
open_and_marks_dirty` and `a_requirements_dependency_can_be_viewed_
removed_and_a_new_one_added` both went back to plain `.click()` for the
form's own Save afterward, confirming it's genuinely on-screen again
rather than needing `.click_accesskit()`'s workaround. Considered and
rejected: gating Save's very presence on field-level change-detection (a
"dirty since load" flag per field) — real complexity (a pristine snapshot
of every loaded value, diffed every frame) for limited payoff, since
Save already requires an explicit click before anything reaches
`gui-core` regardless of whether it's always shown or conditionally
shown.

## Forwards/backwards navigation

A browser-style Back/Forward in the toolbar, ahead of `logical`/`gui-core`
adding real navigation links between requirements, tests, and results (a
result's `requirement_path`/`test_path`, currently just opaque strings in
its form, becoming actual clickable jumps to the target entry) — this
already works for tree clicks today, and is positioned to pick up those
future links for free. Not related to undo/redo (that's a `gui-core`
concept, reverting *edits* — see that crate's README); this is purely
"what did I look at," entirely client-side, `gui-core` has no reason to
know about it at all.

`GuiApp` carries a browser-style position-in-history stack —

```rust
enum NavMode { View, Edit }

struct GuiApp {
    // ...
    nav_history: Vec<(LogicalPath, EntryKind, NavMode)>,
    nav_position: usize,
}
```

(`EntryKind` rides along because `select_from_history` — see below —
needs it to fetch the right pool's detail, same reason `select` itself
needs it. `NavMode` is what makes the viewer/edit split above register
with Back/Forward at all — see below.)

`navigate(target, kind, mode)` is the one chokepoint every current form
of navigation goes through: a tree leaf click (`select`, always
`NavMode::View`), the viewer's "Edit" button (`editor_edit_clicked`,
`NavMode::Edit`), and Cancel-from-an-existing-entry's-edit-form
(`editor_cancel_clicked`, back to `NavMode::View`) all just call it with
the mode they mean. It truncates `nav_history` past `nav_position`,
pushes the new `(target, kind, mode)`, advances `nav_position`, then
delegates the actual selection work to `select_from_history` — the twin
that does everything `navigate` does *except* touch history, which is
what `back_clicked`/`forward_clicked` call directly after moving
`nav_position` themselves. That split matters: a Back click that also
counted as "new navigation" would immediately make Forward available
again pointing at where the user just came from, breaking the usual
back/forward mental model. The moment a future requirement/test/result
navigation link is added, it gets Back/Forward for free *as long as its
own click handler also just calls `navigate()`* rather than inventing its
own way to change `self.selection` — the reason it was kept as the one
chokepoint in the first place, before there was a second and third caller
(the Edit button, Cancel) to retrofit.

Because the viewer and the editable form are reached through the exact
same `GetEntryDetail` round trip (just with a different `NavMode`), Edit
and Cancel are genuinely no different from clicking a different tree
leaf as far as `select_from_history`/`apply_entry_detail` are concerned —
`current_nav_mode()` (reading `nav_history[nav_position]`, set *before*
the request that will eventually land) is the one place that decides
which of the two a completed `GetEntryDetail` reply renders as. The
trade-off is a real round trip on every Edit/Cancel click rather than a
local flag flip — deliberate: it keeps the edit form's starting values
(and the viewer's, after Cancel) always authoritative rather than
trusting whatever's already rendered, so Cancel genuinely discards
unsaved typing rather than displaying it read-only. `gui-core`'s
`GetEntryDetail` is an in-memory `ProjectState` lookup, not real I/O, so
the round trip is cheap in practice.

Toolbar Back/Forward are disabled at either end of `nav_history`
(`can_go_back`/`can_go_forward`, `nav_position > 0` / `nav_position + 1 <
nav_history.len()`) — same "don't offer a click that can only silently
no-op" reasoning the Save/Save As fix established for this crate; purely
a local `Vec`/`usize` check, no `gui-core` round-trip needed.

**Deliberately not done**: module selections (`select_module`) aren't
added to `nav_history` — a module has nothing of its own to show in the
center pane (see `EntryDetail`'s own doc comment on why), so there was
nothing meaningful to navigate back *to*. `nav_history` isn't cleared on
`LoadProject`/`NewProject` (unlike `gui-core`'s undo/redo stacks, which
are, for the same "a project switch invalidates it" reason) — a smaller
gap than it sounds, since a stale entry just re-fetches via
`GetEntryDetail` against whatever project is now loaded and gets
`EntryDetail(None)` back if it no longer exists, rather than anything
worse; left as a known simplification rather than fixed preemptively.
`nav_history` also isn't bounded — unlike `gui-core`'s undo stack (whole
`ProjectState` snapshots), a `(LogicalPath, EntryKind, NavMode)` entry is
tiny, so an unbounded `Vec` growing for an entire session was judged not
worth the added complexity for a first pass. Cancel and Edit always push
a *fresh* history entry rather than trying to smart-detect "the previous
entry already means this, just move `nav_position` back to it" — same
"every click is a forward step" browser-like model a fresh tree click
already follows, at the cost of Back sometimes retracing a step that
feels redundant (e.g. Cancel then Back lands you back on the same edit
form you just cancelled out of, freshly re-fetched) rather than skipping
over it — judged simpler and more predictable than the alternative.

## Debug side panel

A developer/support diagnostic tool, not a product feature. Gated behind
the `debug-panel` Cargo feature *and* `debug_assertions` together — `mod
debug_panel;` in `lib.rs` is `#[cfg(all(feature = "debug-panel",
debug_assertions))]`, same for every other use site (the menu-bar button,
the `GuiApp::debug` field, `send_command`/`poll_events`' interception
points). `debug-panel` is a `default` feature (`Cargo.toml`), so an
ordinary `cargo build`/`cargo run`/`cargo test` (dev profile) compiles it
in with no flag needed; the `debug_assertions` half of the gate is what
actually keeps it out of a `--release` build, since Cargo features alone
have no notion of "on for this profile only" — a `--release` build still
nominally has the feature enabled, but every `cfg(all(...))` site turns
false anyway. `--no-default-features` disables it outright regardless of
profile. See `Cargo.toml`'s own comment on why it must never reach a
release build: the stall/failure triggers it adds are actively harmful in
a real user's hands, not just clutter.

**Opening/closing**: a toggle button in the menu bar's far right corner
(`render_menu_bar`, same `right_to_left` sub-layout technique the status
bar's zoom controls use), labelled "Debug ▼"/"Debug ▲" to reflect state.
The *first* click, with the panel closed, opens a confirmation modal
("Open the debug panel?", Open/Cancel — same shape as every other modal
here) rather than the panel directly; confirming sets `debug.open =
true`. Once open, the *same* button closes it directly — asking "are you
sure" makes sense for opening something diagnostic a normal user
shouldn't stumble into; asking again just to close it would only be
friction (`GuiApp::debug_panel_button_clicked`).

**Layout**: `egui::Panel::right("debug_panel")`, added to `GuiApp::ui`'s
panel sequence right after the left pane, before the `CentralPanel`
(same "panels before the one that fills remaining space" rule governing
every other panel here).

**Contents**, per the three things asked for:

1. **A circular buffer of the `Command`/`Event` traffic between the two
   threads.** Every `Command` `gui-ui` sends funnels through one method,
   `GuiApp::send_command` — all 18 of the crate's own call sites that
   used to call `self.core.send(...)` directly were changed to call this
   instead (`Command::Shutdown`, the exit flow's Stage 2 close, is the
   one deliberate exception — see its own comment on why the debug
   panel must never be able to hold up or drop it). `send_command` routes
   through `DebugPanelState::on_tx`, which logs the command and — Tx
   stall/failure active or not — decides whether to actually forward it
   to `CoreHandle::send`. Likewise every `Event` drains through one
   `GuiApp::poll_events`, replacing the old direct `while let Some(event)
   = self.core.try_recv_event()` loop in `ui()`. `DebugPanelState::log`
   is a bounded `VecDeque<LogEntry>` (`LOG_CAPACITY = 500`, oldest
   dropped once full — same bounded-memory judgment call as `gui-core`'s
   undo stack), each entry `{ at: Instant, direction: Tx | TxDropped |
   Rx, detail: String }` — `detail` from `Command`/`Event`'s own `Debug`
   impl, not a bespoke format, since a diagnostic log's job is
   completeness over prettiness. The panel renders each entry with its
   age (`entry.at.elapsed()`, recomputed fresh every frame so it keeps
   ticking up while the panel stays open) rather than a fixed timestamp.
2. **Local GUI state.** A live, read-only dump of `GuiApp`'s own fields —
   `pending.len()`, `dirty`, `selection`, `selected_module`,
   `project_path`, `nav_history`'s length and position, `exit_dialog` —
   via their own `Debug` impls, in `render_debug_panel`.
3. **Buttons to trigger a stall or failure.** Three of the four are
   implemented:
   - **Tx Stall**: `DebugPanelState::trigger_tx_stall` sets a 3-second
     deadline (`STALL_DURATION`); while active, `on_tx` queues every
     outgoing `Command` in `held_tx` instead of letting `send_command`
     forward it. `GuiApp::flush_stalled_tx`, called once per frame,
     releases the whole queue (in original order) to `CoreHandle::send`
     the moment the deadline passes.
   - **Tx Failure**: `trigger_tx_failure` sets a one-shot flag; the very
     next `on_tx` call drops that one command outright (logged as
     `TxDropped`) and clears the flag — not a standing failure mode.
   - **Rx Stall**: `trigger_rx_stall` sets the same kind of deadline;
     while active, `poll_events` simply returns without draining
     `CoreHandle::try_recv_event` at all, so real events queue up in
     `gui-core`'s own unbounded channel (see its README) rather than
     anything being lost — they all arrive at once the moment the stall
     lifts.
   - **Rx Failure** (an `Event` `gui-core` computed but never sent) is
     *not* implemented — no button for it, just a label explaining why.
     Faithfully reproducing it needs real `gui-core` cooperation
     (something like a debug-only `Command` the actor honors by
     computing the real `Outcome` and discarding it instead of sending
     `Event::Completed`), which would mean adding debug-only surface to
     `gui-core`'s production `Command` enum for a purely diagnostic
     feature — left as an open decision (see `gui-core`'s own
     per-`Command` convention) rather than assumed, per the user's own
     "leave it open for now" call when this was implemented.

All of `DebugPanelState`'s stall/failure/logging logic (`on_tx`,
`log_rx`, `release_stalled_tx`, the `is_*_stalled` checks) is plain,
`egui`-free Rust, unit-tested directly in `debug_panel.rs`'s own `test`
module — no harness needed for any of it. The interaction-level proof
(a real toolbar click actually shows up in the log, the confirm-then-
toggle open/close sequence, a triggered stall's own warning label
appearing) lives in `tests/interaction.rs`, gated the same
`#[cfg(all(feature = "debug-panel", debug_assertions))]` way as the
feature itself — so a default `cargo test -p gui-ui --test interaction`
(dev profile) already runs those two tests; only `--release` or
`--no-default-features` drops them.

## Testing strategy

**Split answer, because "the GUI" is really two different kinds of code
here.** Application *logic* — state transitions, what `Command` a given
action produces, dirty tracking, the exit-dialog state machine — is fully
unit-testable with no window and no `gui-core` running. Actual *rendering*
(does the tree really draw as nested collapsing headers, is the toolbar
icon really where it looks right) is a different kind of test, with a
different, much lower coverage bar.

### Logic: keep it out of `update()`, then it's plain unit tests

The design as sketched already tends this way — `self.dirty`, `self.
exit_dialog: Option<ExitDialogState>`, `self.pending`, `self.editor`,
`self.config` are all plain data, and the interesting behavior (does
clicking Exit with `dirty == true` open the dialog instead of closing the
window; does an `Event::Completed` matching the pending `Saving { request,
.. }` transition to Stage 2; does the deadline elapsing transition to
`TimedOut`) is decidable from that data plus an `Event`/button-press,
without touching `egui::Context` at all. **Concretely, worth enforcing as a
convention**: every such decision lives in a plain method —
`fn on_exit_clicked(&mut self)`, `fn apply_event(&mut self, event: Event)`,
`fn exit_dialog_tick(&mut self, now: Instant)` — that `update()` calls, not
inline in the middle of widget-drawing code. Once that split holds, this is
ordinary Rust unit testing: construct a `GuiApp` (or just the relevant
sub-state), feed it synthetic `Event`s/clicks/`Instant`s, assert on the
resulting state and on what got `send()`-ed to a fake `CoreHandle` (a test
double implementing the same non-blocking send/`try_recv_event` shape —
`CoreHandle` doesn't need to be a trait for production code, but a thin
one for this test seam costs little). `cargo +nightly llvm-cov --branch
-p gui-ui` applies here the same as any other crate — the state-machine
branches (Asking/Saving/TimedOut, dirty/clean, pending/not-pending) are
exactly the kind of thing that discipline already covers well elsewhere in
this workspace.

### Rendering: headless functional smoke tests, adopted (`egui_kittest`)

`egui_kittest` (an [AccessKit](https://accesskit.dev/)-based headless
harness, egui's own official test library) is a dev-dependency, exercised
in `tests/interaction.rs` — 52 tests that run the real `eframe::App::ui`
(plus 2 more, gated behind the `debug-panel` Cargo feature and
`debug_assertions` together — see "Debug side panel" above — that run
under a default `cargo test` dev-profile build already, and only drop out
under `--release` or `--no-default-features`)
against a real `egui::Context`, no window, no GPU: `Harness::new_eframe`
builds a real `GuiApp` (a real `CoreHandle`, so a real actor runs in the
background — not a fake; most tests use the plain `CoreHandle::start()`,
see below for the exceptions), `harness.get_by_role_and_label(...)
.click()` simulates a click on an actual widget, `harness.step()` advances
one frame, and `harness.query_by_role_and_label(...)` inspects the
resulting accessibility tree. Covers: every toolbar button renders, the
four center-pane forms are genuinely distinct (not one form wearing
different headings), Cancel closes a form back to the empty state, the
Attachments modal opens/closes, opening `sample_project` for real
populates the tree, creating a module through the real form marks the
project dirty, the exit dialog's full Asking/Saving/TimedOut/Keep-
waiting/Exit-anyway/Discard/Cancel set all render and resolve correctly
against that real dirty state, selecting a real tree leaf opens it
prefilled in edit mode (with the name field disabled), saving an edit to
an existing requirement keeps the form open and marks the project dirty,
adding a local attachment to an existing requirement appears in its list,
the rename-module dialog renames a real module end to end, both of the
Result form's `ComboBox` path pickers (requirement and test) fill their
field from a real dropdown selection, editing an existing result/test can
add a local attachment (plus, for the test form, a local template file),
the tree's requirements/tests/results grouping (both that the three
folders render and that an empty module grows none of them), the New
Project dialog creates a real blank project and its Cancel button
discards the typed name, a project created from scratch can be
`Validate`d and persisted for the first time, Save/Save As… start
disabled with nothing loaded and enable once a project is (both the
toolbar button and the File menu items — the latter needs the menu
actually open to query, since there's no toolbar button for Save As…
itself), a real click on an enabled Save (with a project whose path is
already known, so nothing here needs a picker) round-trips a genuine
`Command::Save`, the zoom controls: `+`/`−` step by 10 percentage
points and clamp at both the configured floor and ceiling, Reset returns
to 100%, typing a value and pressing Enter applies it, an out-of-range
typed value clamps the same as the buttons do, invalid typed text (not a
number) reverts to the last real value instead of being accepted, and a
change from any of these reaches `gui-config.ron` on disk, Undo/Redo
round-trip a real module creation through `gui-core` and stay disabled
with nothing loaded, and Back/Forward round-trip two real tree
selections (each correctly re-disabled at either end of the history).

**File pickers can't be driven by this test suite at all** — Open
Project…/Save As… pop a real native OS dialog (`rfd`), a separate window
entirely outside `egui`'s own accessibility tree, so no `egui_kittest`
query can see it, click it, or dismiss it. `GuiApp::open_project`/
`GuiApp::save_project_as` are `pub` specifically so tests can call them
directly — `harness.state_mut().open_project(path)` — bypassing the menu
item and its native picker entirely while still exercising everything
downstream of it (the real `Command::LoadProject`/`Command::SaveAs`
round trip through a real `CoreHandle`) exactly as before. Every test
that opens a project does this now; none click "Open Project…" or "Save
As…" — see `open_project_at`'s own comment. This was learned the hard
way, not decided up front: an earlier version of this change left
`open_project_at` clicking through the menu as it always had, and running
the suite popped roughly a dozen real native folder-picker windows on
this session's actual desktop (the same live-display sandbox constraint
noted elsewhere in this project's history) before it was caught and
fixed.

One of these tests (`editing_an_existing_result_can_add_a_local_attachment`)
caught a real bug in `gui-core`, not just exercised already-correct code:
`select()` used to send `GetEntryDetail` with no indication of which kind
was clicked, and the core resolved `target.name` by trying the
requirement/test/result pools in a fixed order — so clicking a result
whose name happened to match a requirement's (a natural pairing;
`sample_project`'s own "definition" requirement and "definition" result
are exactly this) silently opened the *requirement's* detail instead.
Fixed by adding `kind: EntryKind` to `Command::GetEntryDetail` and having
both `render_tree_node` and `select()` pass the clicked node's own kind
through — see `gui-core`'s README for the fuller writeup.

Twelve non-obvious things this surfaced, documented in the test file itself
since they'd otherwise just be silently-wrong test expectations:
- `harness.step()`, never `.run()`/`.run_ok()` — `GuiApp::ui` calls
  `ctx.request_repaint()` unconditionally (see "Never block the render
  thread" above), so `run()`'s "loop until no repaint requested" condition
  never holds and it panics past its step budget every time.
- A click's effect often needs a *second* `step()` — the click's handler
  runs only after the widget that was clicked has already been drawn this
  frame, so a click on a widget in the *same* render function that also
  draws the now-stale content (Cancel inside the very form it closes)
  needs one `step()` to process the click and a second to see the result.
- A newly-opened `egui::Modal` needs one extra `step()` to "settle" before
  its own content is reliably *clickable* (though it's already query-able
  after just one) — found empirically on the Attachments dialog's Close
  button, which otherwise silently no-ops.
- A poll loop waiting on a real cross-thread reply (`LoadProject` etc.)
  needs a real `std::thread::sleep` between attempts, not just repeated
  `step()`s — confirmed empirically: a zero-delay spin loop passed every
  test run alone but started failing once the whole suite ran in parallel
  (enough CPU contention that the real background OS thread doing the
  actual work never got scheduled within the spin loop's attempt budget).
  `tests/interaction.rs`'s `wait_until` helper sleeps 10ms between
  attempts for exactly this reason.
- A `ComboBox`'s trigger reports its selected text as an AccessKit *value*,
  not a *label* (`role=ComboBox, label=None, value=Some("Pick…")`) — found
  by dumping the whole tree once `get_all_by_label` came back empty for
  the trigger. Its dropdown options, once opened, are ordinary labelled
  nodes, but querying by label alone for an option whose text also
  appears elsewhere (e.g. "definition", matching both a real tree leaf
  button *and* the dropdown option) resolves the *first* match in tree
  order, which is the tree's own leaf, not the popup — the popup is a
  separate floating `Area` drawn later in the frame and so later in tree
  order. `get_all_by_label(...).last()` reliably reaches the popup's own
  option; `.next()`/`.first()` clicks the tree's own leaf instead, which —
  while genuinely still a bug to avoid in the test (it silently exercises
  the wrong widget) — is also how the `GetEntryDetail` kind-resolution bug
  above first turned up: clicking that wrong leaf (a *result* named
  "definition") landed on "Edit Requirement", which at the time looked
  like a label-matching problem alone but was actually two overlapping
  issues — a query hitting the wrong node, compounded by the node it hit
  resolving to the wrong entry once clicked.
- `Command::Save` only actually touches the filesystem against a
  `Validated` project — against a `Draft` it fails immediately with
  `SaveError::NotValidated`, with no real I/O at all. A test exercising
  `Save`'s *timing* (not just its outcome) needs to `Validate` first, or
  the "save" it observes is a no-op that resolves near-instantly
  regardless of anything meant to make it slow.
- An `egui::CollapsingHeader`'s own name reports as `Role::Button` (it's
  clickable — toggles expand/collapse), not `Role::Label`, the same as a
  module's own name in the tree — querying `Role::Label` for a
  `CollapsingHeader`'s text (e.g. the new "requirements"/"tests"/
  "results" folders) simply finds nothing, not an error, so this is easy
  to get wrong silently until the assertion fails.
- Two clicks on the *same* button, one `step()` apart, don't reliably
  register as two separate clicks — the zoom tests' second `−` click
  needed three settle steps to show up at all, not the one every other
  same-frame-effect case here needs (found empirically; exact cause not
  fully pinned down, see `zoom_controls_step_by_ten_percent_and_show_
  the_current_level`'s own comment). Distinct from the ordinary "click
  affects an already-drawn widget" lag above, which a single extra step
  always resolves.
- A simulated click *elsewhere* (`Node::click()` on some other, non-text
  widget) doesn't reliably surrender a focused singleline `TextEdit`'s
  focus in the harness, even though a real click away from a text field
  always blurs it in normal use — tried first for the zoom field's
  commit-on-blur tests and it silently left the field's own (invalid,
  in-progress) text unsubmitted every time. `harness.key_press(egui::
  Key::Enter)` after focusing/typing works reliably instead, and is
  arguably more correct anyway: `egui`'s own source (`text_edit/
  builder.rs`) documents that Enter both commits a singleline field's
  value *and* explicitly surrenders its focus, which is exactly the
  `response.lost_focus()` `render_status_bar` checks for.
- The status bar's zoom field is *always* present, on every screen — so
  every other test's `Role::TextInput` query that used to be able to
  assume "the only one" (a dialog's or form's own single field) broke the
  moment it was added, all at once, across a dozen previously-unrelated
  tests. Since the status bar always renders before the left/center pane
  or any dialog in `ui()`, the zoom field is reliably *first* in tree
  order — every affected lookup was fixed by either `.last()` (single
  other field expected) or a uniform `+1` to every existing `.nth(...)`
  index, not by anything test-specific. Worth remembering before adding
  another always-present widget with the same role as something forms
  already query for.
- A plain `ui.horizontal` silently overflows rather than reporting an
  error when its content is wider than the available space — found when
  adding Undo/Redo/Back/Forward pushed the toolbar past `egui`'s own
  merely-800px-wide default test window, and the "Attachments…" button
  (now positioned partway past the right edge) simply stopped being
  clickable, no warning anywhere. `harness.get_by_role_and_label(...)
  .rect()` is what actually confirmed it — the button's own reported
  rect extended well past the 800px viewport. Fixed for real (not just
  in the test) by switching the toolbar to `ui.horizontal_wrapped`.
- Raising `egui`'s own zoom factor *shrinks* how many logical points fit
  in a fixed physical window — the opposite of what "zoom in" suggests
  at first: physical pixels are constant, so more of them go to each
  logical point, leaving room for fewer of them. At this crate's
  configured zoom ceiling (400%) `egui_kittest`'s 800×600 default leaves
  only ~200 logical points wide, not enough for the status bar's own
  zoom controls to all fit — `zoom_in_stops_at_the_configured_maximum`
  (which has to click `+` repeatedly, ending up at high zoom while still
  needing to click it) needs a wider-than-default harness
  (`Harness::builder().with_size(...)`) to avoid clicking itself off its
  own visible area partway through. A real, resizable window wouldn't
  hit this at any remotely reasonable size; only a fixed-size test
  viewport pushed to this crate's own zoom ceiling does.

**What's not realistic to chase, even with the harness adopted**: 100%,
or even high, branch coverage over the rendering code itself. Immediate-
mode UI code is mostly straight-line widget-building calls (`ui.label(...)`,
`ui.button(...)`) with comparatively few real branches, and the branches
egui's own internals take (layout, hit-testing, painting) aren't this
crate's code to cover in the first place. The `disk`-crate-style 100%
branch coverage bar makes sense for `logical`/`gui-core`'s decision-dense
state machines; holding `gui-ui`'s *rendering* to the same bar would
mostly mean writing tests whose only job is satisfying the metric. The 52
tests here are deliberately a functional smoke layer, growing toward
covering every real state transition rather than every widget and dialog
exhaustively at once.

## Open questions

- Keyboard navigation / accessibility for the tree and forms — not
  addressed yet, deferred until the layout above is actually built.
- `egui_kittest` interaction coverage now covers every documented state
  transition, including the exit dialog's full Asking/Saving/TimedOut/
  Keep-waiting/Exit-anyway/Discard/Cancel set, a real dirty-project round
  trip, editing an existing requirement/test/result (open-prefilled,
  disabled name field, save-keeps-form-open), adding a local attachment
  to an existing requirement/result and a local attachment plus template
  file to an existing test, the rename-module dialog end to end, and both
  of the Result form's path pickers. Reaching `Saving`/`TimedOut`
  deterministically needed a real fix, not just a short timeout: a
  `save_on_exit_timeout` of zero was tried first, on the theory that a
  same-process deadline check would always beat a real cross-thread
  completion — that didn't hold, since a real `Save` against
  `sample_project` (pure local disk I/O, no git shell-out the way
  `Validate` needs) sometimes completed faster than `egui_kittest`'s own
  `step()` call returns, winning the race to `Ready` before `TimedOut` was
  ever observable, so that version was flaky and got reverted. The actual
  fix: `gui-core` now exposes `CoreHandle::start_with(fs, git)` (its
  already-generic `Actor<F, G>` as public API), and `syscalls` gained a
  `SlowFilesystem<F>` wrapper that sleeps before every `write`/
  `create_dir_all` — reads stay fast, so `LoadProject` is unaffected and
  only `Save` is deliberately slow. Since that test is also the one test
  here that completes a real `Save`, it runs against
  `tests/interaction.rs`'s own `scratch_copy_of_sample_project` (mirroring
  `gui-core`'s own convention of the same name), never the real fixture —
  an earlier version of the test pointed straight at `sample_project` and
  a completed `Save` permanently wrote a new module and reformatted every
  `.ron` file into the repository's own working tree, caught and reverted
  before landing. It also needs its own `Git`/`RemoteGit` fake
  (`tests/interaction.rs`'s own `FixedGit`, mirroring `gui-core`'s
  private one of the same name): a scratch copy lives outside any git
  repository, so the real `syscalls::SystemGit` would fail every commit
  lookup during `LoadProject`/`Validate` against it. "Test every kind of
  state transition" was the standing goal driving all of this — it's now
  met for every transition this crate's design docs call out.
- `egui_kittest`'s `snapshot`/`wgpu` features (pixel-level screenshot
  regression testing) aren't enabled — only interaction/query testing is,
  which needs no rendering backend at all and stays fully headless in a
  sandbox with no reliable GPU access.
