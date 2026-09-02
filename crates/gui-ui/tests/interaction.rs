//! Headless interaction tests via `egui_kittest` — the piece
//! `README.md`'s Testing strategy names but had never actually adopted:
//! everything in `src/lib.rs`'s own `#[cfg(test)]` module exercises
//! `GuiApp`'s plain logic methods directly, never `eframe::App::ui`
//! itself. These tests run the real rendering code (`view.rs`'s
//! `render_*` methods) against a real `egui::Context`, with no window and
//! no GPU — `egui_kittest`'s `Harness` simulates clicks/typing and steps
//! frames entirely in memory.
//!
//! **`harness.step()`, never `.run()`/`.run_ok()`**: `GuiApp::ui` calls
//! `ctx.request_repaint()` unconditionally every frame (see README's
//! "Never block the render thread" — it has to, to keep polling
//! `CoreHandle::try_recv_event` promptly). `Harness::run()` loops until no
//! repaint is requested and panics past its step budget, which a
//! never-stops-requesting-repaints app always exceeds. `step()` advances
//! exactly one frame regardless, which is all a click-then-assert test
//! needs.
//!
//! **A click's effect often needs a *second* `step()` to become
//! visible**: immediate-mode rendering draws top-to-bottom within one
//! frame, and a click's handler (`self.editor_cancel_clicked()` etc.)
//! only runs *after* the widget that was clicked has already been drawn.
//! So a click on a widget belonging to the *same* render function that
//! also draws the now-stale content (e.g. the Cancel button inside the
//! very form it closes) needs one `step()` to process the click and a
//! second to see the updated render. A click in an *earlier*-rendered
//! region affecting a *later* one within the same frame (e.g. a toolbar
//! button opening a center-pane form) is visible after just one `step()`,
//! since the later region hasn't drawn yet when the click is handled.
//! Found empirically while writing these — not something the design docs
//! called out ahead of time.
//!
//! **Buttons and headings sharing exact text need `..._by_role_and_label`,
//! not `..._by_label`**: e.g. the "New Module" toolbar button and the
//! "New Module" form heading have identical text but different
//! `accesskit::Role`s (`Button` vs. `Label`) — `query_by_label` alone is
//! ambiguous between them and panics.
//!
//! **A newly-opened `egui::Modal` needs one extra `step()` before its own
//! content is reliably clickable** — confirmed empirically in
//! `close_button_closes_the_attachments_dialog`: a click on "Close"
//! (found fine by the query, genuinely inside the modal) silently did
//! nothing until a second `step()` ran between the modal opening and the
//! click. Querying the modal's content is fine after the first `step()`;
//! interacting with it needs the modal to "settle" for one frame first.
//! `render_open_project_dialog`/`render_exit_dialog` are also `egui::Modal`s
//! and presumably share this, though it's only actually exercised here for
//! the Attachments dialog.

use std::path::{Path, PathBuf};
use std::time::Duration;

use accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use gui_ui::{GuiApp, GuiConfig, RecentProjects};

fn harness<'a>() -> Harness<'a, GuiApp> {
    // `/dev/null`: no test built on this helper exercises zoom
    // persistence — see `config::test` in `src/config.rs` for that.
    Harness::new_eframe(|_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start(),
            GuiConfig::default(),
            PathBuf::from("/dev/null"),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    })
}

/// Waits for `condition` to become true, `step()`-ing the harness with a
/// real sleep between attempts. Real wall-clock sleep matters here, not
/// just repeated `step()`s: the thing being waited for (a `LoadProject`/
/// `Command` reply) completes on a genuinely separate OS thread (`gui-
/// core`'s real background actor, including real `git` subprocess calls
/// for commit lookups) — a zero-delay spin loop can burn through its
/// entire attempt budget before that other thread ever gets scheduled,
/// especially under the CPU contention of running many of these tests in
/// parallel (confirmed empirically: this exact flakiness showed up only
/// when running the full suite together, never when running one test
/// alone). Bounded so a real hang still fails the test instead of the
/// suite, per README's "Known gap" about `validate()` having no timeout —
/// same shape of problem, this is gui-ui's side of coping with it.
fn wait_until(harness: &mut Harness<GuiApp>, mut condition: impl FnMut(&mut Harness<GuiApp>) -> bool) {
    for _ in 0..200 {
        if condition(harness) {
            return;
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition was never met within the step/wait budget");
}

/// A `file://` remote URL for the local git repository at `dir` — same
/// trick `syscalls`' own tests use so `RemoteGit::commit_for_remote` can
/// be exercised for real (a real `git clone`/`ls-remote`) without any
/// network access.
fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

/// Opens this repo's own `sample_project` fixture (the same one
/// `gui-core`'s tests use) against the real `CoreHandle`'s real
/// background actor — not a fake.
fn open_sample_project(harness: &mut Harness<GuiApp>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project");
    open_project_at(harness, &path);
}

/// Same as `open_sample_project`, but against a caller-supplied path —
/// for a test that will actually complete a real `Save`, which must run
/// against `scratch_copy_of_sample_project`'s writable copy, never the
/// real fixture (see that function's own doc comment).
///
/// Calls `GuiApp::open_project` directly rather than going through the
/// real File -> Open Project… menu item, which — since the file picker
/// change — pops a real native OS folder picker (`rfd`) that no headless
/// test harness can drive or dismiss; clicking it here previously hung
/// every test that opened a project and, worse, popped a real dialog on
/// whatever display this process happens to be running against. See
/// `GuiApp::open_project`'s own doc comment: it's `pub` specifically so
/// tests can reach it without the UI in between.
fn open_project_at(harness: &mut Harness<GuiApp>, path: &Path) {
    harness.state_mut().open_project(path.to_path_buf());
    harness.step();

    wait_until(harness, |h| h.query_by_label("No project loaded.").is_none());

    // Every `CollapsingHeader` in the tree (modules and the
    // requirements/tests/results leaf groups alike) now starts collapsed
    // on open — see `render_tree_node`/`render_leaf_group`'s own
    // `default_open(false)`. The tests below exercise leaf-clicking and
    // other tree interactions that assume everything is already visible,
    // not the collapsed-by-default behavior itself (that's its own test,
    // `the_tree_starts_fully_collapsed_when_a_project_first_opens`), so
    // expand everything here once, right after load, via the same
    // "Expand All" button a real user would click.
    harness.get_by_role_and_label(Role::Button, "Expand All").click();
    harness.step();
    harness.step();
}

/// Clicks a tree leaf by its button label, waits for its read-only
/// viewer, then clicks through the viewer's own "Edit" button to reach
/// the editable form — the real two-step path a user now takes to edit
/// an existing entry (see `GuiApp::editor_edit_clicked`, and
/// `selecting_an_existing_requirement_opens_its_read_only_viewer`, which
/// tests this exact transition on its own). Every test that only cares
/// about the *editable* form (not the viewer-to-editor switch itself)
/// uses this to get there without re-proving the transition each time.
/// `heading` is the edit-mode heading to wait for (e.g. "Edit
/// Requirement").
fn open_leaf_for_editing(harness: &mut Harness<GuiApp>, leaf_label: &str, heading: &str) {
    harness.get_by_role_and_label(Role::Button, leaf_label).click();
    harness.step();
    wait_until(harness, |h| h.query_by_role_and_label(Role::Button, "Edit").is_some());
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(harness, |h| h.query_by_role_and_label(Role::Label, heading).is_some());
}

/// A writable copy of `sample_project`, so a test that actually completes
/// a `Save` doesn't write back into the repository's own fixture — same
/// convention (and same reason) as `gui-core`'s own
/// `scratch_copy_of_sample_project` in `crates/gui-core/src/actor.rs`'s
/// test module. Caller is responsible for `remove_dir_all`.
fn scratch_copy_of_sample_project(label: &str) -> PathBuf {
    let sample_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project");
    let dest = std::env::temp_dir().join(format!("gui-ui-interaction-test-{label}-{}", std::process::id()));
    std::fs::remove_dir_all(&dest).ok();
    let status = std::process::Command::new("cp")
        .args(["-r", sample_project.to_str().unwrap(), dest.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to copy sample_project to {dest:?}");
    dest
}

/// A fixed-commit `Git`/`RemoteGit` fake — same shape as (and same reason
/// for) `gui-core`'s own private `FixedGit` in `actor.rs`'s test module,
/// duplicated here since that one isn't `pub` outside its crate. A
/// `scratch_copy_of_sample_project` copy lives outside any git repository
/// (`cp -r` doesn't carry `.git` along, and `/tmp` isn't inside this
/// repo's working tree either), so the real `syscalls::SystemGit` would
/// fail every commit lookup during `LoadProject`/`Validate` against it —
/// this fake sidesteps that; the commit values it returns don't matter
/// for a test that isn't inspecting them.
#[derive(Debug, Clone, Copy, Default)]
struct FixedGit;

impl syscalls::Git for FixedGit {
    fn commit_for_path_excluding(
        &self,
        _path: &Path,
        _excludes: &[&Path],
    ) -> Result<String, syscalls::CommitForPathError> {
        Ok("deadbeef".to_string())
    }
}

impl syscalls::RemoteGit for FixedGit {
    fn commit_for_remote(&self, _url: &str, _path: Option<&Path>) -> Result<String, syscalls::CommitForRemoteError> {
        Ok("deadbeef".to_string())
    }
}

#[test]
fn the_status_bar_reports_no_project_loaded_before_anything_is_open() {
    let mut harness = harness();
    harness.step();

    // Distinct from the center pane's own "No project loaded." message
    // (with a period) — this is the status bar's text, which used to
    // unconditionally show "saved" here (`self.dirty` defaults `false`),
    // misleadingly implying a project existed and had been saved.
    assert!(harness.query_by_label("No project loaded").is_some());
    assert!(harness.query_by_label("saved").is_none());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_none());
}

#[test]
fn new_project_menu_creates_a_blank_project_and_marks_it_saved() {
    let mut harness = harness();
    harness.step();
    assert!(harness.query_by_label("No project loaded.").is_some());

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step(); // let the menu popup settle before clicking into it.

    harness.get_by_role_and_label(Role::Button, "New Project…").click();
    harness.step();
    harness.step(); // let the modal settle — see this file's module doc.
    assert!(harness.query_by_role_and_label(Role::Label, "New Project").is_some());

    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness.get_all_by_role(Role::TextInput).last().expect("name field not found");
    name_field.focus();
    name_field.type_text("Scratch Project");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Create").click();
    harness.step();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("No project loaded.").is_none());
    // The project's own name, from what was just typed — proves a real
    // `Command::NewProject` round-tripped through the actor and came back
    // as a real `Event::TreeChanged`, not just that the empty-state
    // message went away.
    assert!(harness.query_by_label("Scratch Project").is_some());
    // A brand new, never-saved project starts clean, same as a freshly
    // loaded one — nothing unsaved to lose yet (see `apply_outcome`'s
    // `Outcome::NewProject` arm).
    assert!(harness.query_by_label("saved").is_some());
    // Empty — no requirements/tests/results folders at all (same check
    // as `an_empty_module_shows_no_leaf_group_folders`, but for the
    // project root this time).
    assert!(harness.query_by_role_and_label(Role::Button, "requirements").is_none());
}

#[test]
fn new_project_dialog_cancel_discards_the_typed_name() {
    let mut harness = harness();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "New Project…").click();
    harness.step();
    harness.step();

    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness.get_all_by_role(Role::TextInput).last().expect("name field not found");
    name_field.focus();
    name_field.type_text("Abandoned");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Cancel").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "New Project").is_none());
    assert!(harness.query_by_label("No project loaded.").is_some());
}

#[test]
fn new_project_then_save_as_creates_and_persists_a_project_from_scratch() {
    // `syscalls::StdFilesystem` + `FixedGit` — a real `Save` needs a real
    // filesystem, but nothing here has a remote reference for `Validate`
    // to actually resolve, so `FixedGit` never even gets called; it's
    // just here so `CoreHandle::start_with` has *some* `Git`/`RemoteGit`
    // to plug in (its bound needs both, same as every other test using
    // `start_with`).
    let core = gui_core::CoreHandle::start_with(syscalls::StdFilesystem, FixedGit);
    let mut harness = Harness::new_eframe(|_cc| {
        GuiApp::new(
            core,
            GuiConfig::default(),
            PathBuf::from("/dev/null"),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    });
    harness.step();

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "New Project…").click();
    harness.step();
    harness.step();
    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness.get_all_by_role(Role::TextInput).last().expect("name field not found");
    name_field.focus();
    name_field.type_text("Scratch Project");
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Create").click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Scratch Project").is_some());

    harness.get_by_role_and_label(Role::Button, "New Requirement").click();
    harness.step();
    // Field order among `Role::TextInput` nodes in create mode: the
    // status bar's own zoom field(0) and the left pane's own filter
    // field(1) — both always first, rendered before the center pane —
    // then name(2), title(3).
    let fields: Vec<_> = harness.get_all_by_role(Role::TextInput).collect();
    fields[2].focus();
    fields[2].type_text("scratch");
    fields[3].focus();
    fields[3].type_text("Scratch Requirement");
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Create").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("\u{e18a} unsaved changes").is_some());

    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());

    let dir = std::env::temp_dir().join(format!("gui-ui-interaction-test-new-project-save-as-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    // Called directly rather than through the real "Save As…" menu item,
    // which pops a real native OS folder picker — see `open_project_at`'s
    // own comment on why `GuiApp::save_project_as` is `pub` for exactly
    // this reason.
    harness.state_mut().save_project_as(dir.clone());
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("saved").is_some());

    assert!(dir.join("requirements/scratch/requirement.ron").exists());

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn opening_a_project_records_it_to_recent_ron_and_it_appears_in_the_file_menu() {
    let dir = std::env::temp_dir().join(format!("gui-ui-interaction-test-recent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let recent_path = dir.join("recent.ron");

    let sample_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project");
    let sample_project_for_app = sample_project.clone();
    let recent_path_for_app = recent_path.clone();
    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start(),
            GuiConfig::default(),
            PathBuf::from("/dev/null"),
            RecentProjects::default(),
            recent_path_for_app.clone(),
        )
    });
    harness.step();
    open_project_at(&mut harness, &sample_project_for_app);

    // `open_project_at`'s own `wait_until` only waits for `Event::
    // TreeChanged` to land (the signal "No project loaded." watches) —
    // the *separate* `Event::Completed { outcome: LoadProject(Ok(())),
    // .. }` that actually triggers `record_recent_project` is a second,
    // independent send from the same background actor, so it can still
    // be in flight for a step or two after the tree itself is visible
    // (same cross-thread-timing class of flakiness `wait_until`'s own
    // doc comment already calls out). Polling the real file directly,
    // bounded, rather than assuming one extra `step()` is always enough.
    let mut recent = RecentProjects::default();
    for _ in 0..50 {
        let (loaded, error) = RecentProjects::load(&recent_path);
        assert!(error.is_none());
        if !loaded.paths.is_empty() {
            recent = loaded;
            break;
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(recent.paths, vec![sample_project.clone()]);

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    // egui appends a "▶"-style submenu arrow glyph to a nested
    // `menu_button`'s own label automatically — found empirically (a
    // plain "Open Recent" query came back empty; dumping every button's
    // label showed "Open Recent ⏵").
    harness.get_by_role_and_label(Role::Button, "Open Recent ⏵").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_label(sample_project.display().to_string().as_str()).is_some());

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn clicking_a_recent_project_loads_it() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Capstone").is_some());

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Open Recent ⏵").click();
    harness.step();
    harness.step();

    let sample_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project");
    harness.get_by_role_and_label(Role::Button, sample_project.display().to_string().as_str()).click();
    harness.step();

    // The project was already loaded — re-clicking its own recent entry
    // re-loads the same path, a real (if redundant) `LoadProject` round
    // trip, not a no-op; "Capstone" reappearing after the tree
    // momentarily clears confirms a real reload happened, not that the
    // click did nothing.
    wait_until(&mut harness, |h| h.query_by_label("Capstone").is_some());
    assert!(harness.query_by_label("Capstone").is_some());
}

#[test]
fn unsaved_changes_prompts_before_new_project_and_cancel_leaves_everything_alone() {
    let mut harness = dirty_harness();

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "New Project…").click();
    harness.step();
    harness.step();

    // The confirmation prompt, not the name-entry dialog straight away.
    assert!(harness.query_by_label("You have unsaved changes. Continue and lose them?").is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "New Project").is_none());

    harness.get_by_role_and_label(Role::Button, "Cancel").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("You have unsaved changes. Continue and lose them?").is_none());
    assert!(harness.query_by_role_and_label(Role::Label, "New Project").is_none());
    // Still the original, still-dirty project — Cancel didn't discard
    // anything.
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
    assert!(harness.query_by_label("Capstone").is_some());
}

#[test]
fn unsaved_changes_continue_opens_the_new_project_dialog() {
    let mut harness = dirty_harness();

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "New Project…").click();
    harness.step();
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Continue").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("You have unsaved changes. Continue and lose them?").is_none());
    assert!(harness.query_by_role_and_label(Role::Label, "New Project").is_some());
}

#[test]
fn the_initial_layout_renders_every_toolbar_button_and_empty_state_messages() {
    let mut harness = harness();
    harness.step();

    // Every one of these renders icon-only now (`icons.rs`/`icon_button` —
    // see that function's own doc comment), so finding each by this exact
    // *old* text is also a regression test for the accessible-name
    // override: if that override ever stopped working, egui would derive
    // each button's accessible name from its icon glyph instead, and
    // every one of these lookups would start failing here.
    for label in [
        "Save",
        "Validate",
        "Undo",
        "Redo",
        "Back",
        "Forward",
        "New Requirement",
        "New Test",
        "New Result",
        "New Module",
        "Attachments…",
    ] {
        assert!(
            harness.query_by_role_and_label(Role::Button, label).is_some(),
            "missing toolbar button {label:?}"
        );
    }
    assert!(harness.query_by_label("No project loaded.").is_some());
    assert!(
        harness
            .query_by_label("Select an entry in the tree to view it, or use the toolbar to create a new one.")
            .is_some()
    );
}

#[test]
fn the_file_menus_icon_buttons_keep_their_old_accessible_names() {
    let mut harness = harness();
    harness.step();

    // `icon_text_button` (`view.rs`) is a *different* code path from the
    // toolbar's `icon_button` — icon-plus-text rather than icon-only, but
    // the same accessible-name-override mechanism, so worth its own
    // direct check rather than assuming the toolbar test above covers it.
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();

    // Not `query_by_role_and_label` (which requires uniqueness) — "Save"
    // also matches the toolbar's own same-named button, still present (if
    // disabled) behind the open menu. "Exit" has no toolbar counterpart
    // (removed as a redundant, easy-to-misclick duplicate of this File
    // menu item), but stays in this `query_all` loop along with the rest.
    for label in ["New Project…", "Open Project…", "Save", "Save As…", "Exit"] {
        assert!(
            harness.query_all_by_role_and_label(Role::Button, label).next().is_some(),
            "missing File menu item {label:?}"
        );
    }
}

/// The zoom text field's current value — it's the only `Role::TextInput`
/// present in every zoom test here (no project/form open, so no other
/// text field exists to collide with).
fn zoom_field_value(harness: &Harness<GuiApp>) -> String {
    // `.nth(0)`, not `.get_by_role` (which requires uniqueness): the
    // left pane's own filter field (see `render_left_pane`) is always
    // present too, but the zoom field — status bar, which renders
    // before the left pane in `ui()` — is reliably first in tree order.
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("zoom field not found")
        .value()
        .expect("zoom field has no value")
}

#[test]
fn zoom_controls_step_by_ten_percent_and_show_the_current_level() {
    let mut harness = harness();
    harness.step();
    assert_eq!(zoom_field_value(&harness), "100");

    harness.get_by_role_and_label(Role::Button, "+").click();
    harness.step();
    assert_eq!(zoom_field_value(&harness), "110");

    // `−` is coded *after* the `+`/value/`%` in `render_status_bar`
    // (`right_to_left` flips the *visual* order, but code order — and so
    // click-then-redraw order within one frame — stays `+`, `%`, value
    // field, `−`, Reset; see that fn's own comment), so its own click
    // affects a field that was already drawn earlier the same frame —
    // the same "click affects a widget already drawn this frame" shape
    // as this file's module doc, unlike `+`, which is coded first.
    // Two consecutive `−` clicks each just one `step()` apart needed
    // *three* settle steps here, not the one the module doc's other
    // examples need — found empirically, exact cause not fully pinned
    // down (a second same-widget click in immediate succession needing
    // extra settling to be recognized as separate from the first is the
    // leading theory) — three is what reliably worked across repeated
    // runs.
    harness.get_by_role_and_label(Role::Button, "−").click();
    harness.step();
    harness.step();
    harness.step();
    assert_eq!(zoom_field_value(&harness), "100");

    harness.get_by_role_and_label(Role::Button, "−").click();
    harness.step();
    harness.step();
    harness.step();
    assert_eq!(zoom_field_value(&harness), "90");
}

#[test]
fn zoom_out_stops_at_the_configured_minimum() {
    let mut harness = harness();
    harness.step();

    // Zoom starts at 100% and the floor is 80% (10% steps), so 5 clicks
    // is more than enough to prove it clamps rather than going negative
    // or past the floor. Three settle steps per click — see
    // `zoom_controls_step_by_ten_percent_and_show_the_current_level`'s
    // own comment on why `−` specifically needs that many.
    for _ in 0..5 {
        harness.get_by_role_and_label(Role::Button, "−").click();
        harness.step();
        harness.step();
        harness.step();
    }

    assert_eq!(zoom_field_value(&harness), "80");
}

#[test]
fn zoom_in_stops_at_the_configured_maximum() {
    // A wider-than-default viewport: `egui`'s zoom factor shrinks how
    // many *logical* points fit in a fixed *physical* window (the
    // opposite of what "zoom in" sounds like at first — physical pixels
    // are constant, so more of them go to each logical point, leaving
    // room for fewer of them) — at 400%, `egui_kittest`'s own 800×600
    // default leaves only ~200 logical points wide, not enough to fit
    // the status bar's own right-aligned zoom controls, so clicking `+`
    // repeatedly eventually starts clicking nothing at all once it
    // pushes itself off the shrunk visible area. A real, resizable
    // window wouldn't hit this at a remotely reasonable size; this test
    // just needs a wide enough fixed one to match.
    let mut harness = Harness::builder().with_size(egui::Vec2::new(1600.0, 600.0)).build_eframe(|_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start(),
            GuiConfig::default(),
            PathBuf::from("/dev/null"),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    });
    harness.step();

    // Zoom starts at 100% and the ceiling is 400% (10% steps), so 35
    // clicks is more than enough to prove it clamps at the ceiling.
    for _ in 0..35 {
        harness.get_by_role_and_label(Role::Button, "+").click();
        harness.step();
    }

    assert_eq!(zoom_field_value(&harness), "400");
}

#[test]
fn zoom_reset_returns_to_one_hundred_percent() {
    let mut harness = harness();
    harness.step();

    harness.get_by_role_and_label(Role::Button, "+").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "+").click();
    harness.step();
    assert_eq!(zoom_field_value(&harness), "120");

    // Reset is coded last of the four zoom controls (`+`, `%`, value
    // field, `−`, Reset — see `render_status_bar`'s own comment), so —
    // same reasoning as `−` needing three settle steps in
    // `zoom_controls_step_by_ten_percent_and_show_the_current_level` —
    // its click affects a field already drawn earlier the same frame.
    harness.get_by_role_and_label(Role::Button, "Reset").click();
    harness.step();
    harness.step();
    harness.step();
    assert_eq!(zoom_field_value(&harness), "100");
}

#[test]
fn typing_a_zoom_percent_and_pressing_enter_applies_it() {
    let mut harness = harness();
    harness.step();

    // `.next()`, not `.get_by_role`: the filter field is always
    // present too now — see `zoom_field_value`'s own comment.
    let field = harness.get_all_by_role(Role::TextInput).next().expect("zoom field not found");
    field.focus();
    // The field starts with "100" already in it (kept in sync with the
    // real current value — see `GuiApp::sync_zoom_input`); `type_text`
    // inserts at the cursor rather than replacing it, so this test
    // works with whatever ends up typed rather than fighting that.
    field.type_text("50");
    harness.step();

    // Enter commits a singleline `TextEdit`'s value *and* surrenders its
    // focus (confirmed in `egui`'s own source,
    // `text_edit/builder.rs`: "Pressing enter key will result in the
    // `TextEdit` losing focus") — exactly the `response.lost_focus()`
    // `render_status_bar` checks to call `zoom_input_submitted`.
    harness.key_press(egui::Key::Enter);
    harness.step();

    // "10050" clamped to the configured ceiling.
    assert_eq!(zoom_field_value(&harness), "400");
}

#[test]
fn typing_an_out_of_range_zoom_percent_clamps_to_the_configured_bounds() {
    let mut harness = harness();
    harness.step();

    // `.next()`, not `.get_by_role`: the filter field is always
    // present too now — see `zoom_field_value`'s own comment.
    let field = harness.get_all_by_role(Role::TextInput).next().expect("zoom field not found");
    field.focus();
    field.type_text("1");
    harness.step();
    harness.key_press(egui::Key::Enter);
    harness.step();

    // "1001" clamped down to the ceiling, not left as an out-of-range
    // value or silently ignored.
    assert_eq!(zoom_field_value(&harness), "400");
}

#[test]
fn typing_invalid_zoom_text_reverts_to_the_last_real_value() {
    let mut harness = harness();
    harness.step();

    // `.next()`, not `.get_by_role`: the filter field is always
    // present too now — see `zoom_field_value`'s own comment.
    let field = harness.get_all_by_role(Role::TextInput).next().expect("zoom field not found");
    field.focus();
    // Not a number at all — `zoom_input_submitted`'s parse fails, so
    // this exercises the "reject and revert" branch specifically, not
    // just clamping.
    field.type_text("abc");
    harness.step();
    harness.key_press(egui::Key::Enter);
    harness.step();

    // Reverted to the real (unchanged) current value, not left showing
    // "100abc" or similar.
    assert_eq!(zoom_field_value(&harness), "100");
}

#[test]
fn a_zoom_change_persists_to_the_config_file() {
    let dir = std::env::temp_dir().join(format!("gui-ui-interaction-test-zoom-persist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("gui-config.ron");
    let config_path_for_app = config_path.clone();

    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start(),
            GuiConfig::default(),
            config_path_for_app.clone(),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    });
    harness.step();

    harness.get_by_role_and_label(Role::Button, "+").click();
    harness.step();

    let (loaded, error) = GuiConfig::load(&config_path);
    assert!(error.is_none());
    assert_eq!(loaded.zoom_percent, 110);

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_theme_selector_defaults_to_system_and_offers_light_and_dark() {
    let mut harness = harness();
    harness.step();

    // The `ComboBox`'s trigger reports its selected text as an AccessKit
    // *value*, same convention as the (now-retired) path pickers used to
    // — see `render_status_bar`'s own comment on why this sits farthest
    // in from the left, ahead of everything else in the bar.
    assert!(harness.get_all_by_value("System").next().is_some());

    harness.get_all_by_value("System").next().unwrap().click();
    harness.step();
    harness.step(); // let the popup settle, same as every other `ComboBox` test here.

    assert!(harness.query_by_label("Light").is_some());
    assert!(harness.query_by_label("Dark").is_some());
    assert!(harness.query_by_label("System").is_some());
}

#[test]
fn selecting_a_theme_updates_the_selector_and_persists_to_the_config_file() {
    let dir = std::env::temp_dir().join(format!("gui-ui-interaction-test-theme-persist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("gui-config.ron");
    let config_path_for_app = config_path.clone();

    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start(),
            GuiConfig::default(),
            config_path_for_app.clone(),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    });
    harness.step();

    harness.get_all_by_value("System").next().expect("theme selector not found").click();
    harness.step();
    harness.step();

    harness.get_by_label("Dark").click();
    harness.step();
    harness.step();

    assert!(harness.get_all_by_value("Dark").next().is_some());

    let (loaded, error) = GuiConfig::load(&config_path);
    assert!(error.is_none());
    assert_eq!(loaded.theme, gui_ui::ThemeChoice::Dark);

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_is_disabled_with_no_project_loaded_and_enables_once_one_is() {
    let mut harness = harness();
    harness.step();

    let save_button = harness.get_by_role_and_label(Role::Button, "Save");
    assert!(save_button.accesskit_node().is_disabled(), "Save should start disabled with nothing loaded");

    // "Save As…" only exists in the File menu (no toolbar button for
    // it), so checking it needs the menu open — see this file's module
    // doc on the two-step "let the popup settle" pattern.
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    let save_as_button = harness.get_by_role_and_label(Role::Button, "Save As…");
    assert!(save_as_button.accesskit_node().is_disabled(), "Save As… should start disabled with nothing loaded");

    // Close the menu again — left open, its own "Save" item would make
    // the toolbar's "Save" query below ambiguous (both share the exact
    // role+label).
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();

    open_sample_project(&mut harness);

    let save_button = harness.get_by_role_and_label(Role::Button, "Save");
    assert!(!save_button.accesskit_node().is_disabled(), "Save should enable once a project is loaded");

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    let save_as_button = harness.get_by_role_and_label(Role::Button, "Save As…");
    assert!(!save_as_button.accesskit_node().is_disabled(), "Save As… should enable once a project is loaded");
}

#[test]
fn clicking_save_with_a_known_path_sends_a_real_save_command() {
    // `open_sample_project` gives this a real, already-known path (the
    // real `LoadProject` it sends really does complete and populate
    // `project_path` — see `apply_project_path_result`), so
    // `save_button_clicked` takes its non-picker branch here and this
    // click never touches `rfd` — safe to actually click in a headless
    // test, unlike Save with nothing loaded (which would fall back to a
    // real native picker if it weren't disabled) or Save As (which
    // always does).
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "Save").click();
    harness.step();

    // Never validated, so the real `Command::Save` this sent comes back
    // `Err(SaveError::NotValidated)` — not surfaced anywhere in the UI
    // (same as every other `Save`/`SaveAs` failure, see `apply_outcome`'s
    // catch-all), so there's nothing to assert about the *outcome*
    // itself. What this proves is narrower but still real: the click
    // didn't panic, didn't pop a picker, and the app is still showing
    // the loaded project afterward, not stuck on some broken state.
    assert!(harness.query_by_label("No project loaded.").is_none());
    assert!(harness.query_by_label("Capstone").is_some());
}

#[test]
fn new_module_button_opens_a_blank_creation_form() {
    let mut harness = harness();
    harness.step();

    harness.get_by_role_and_label(Role::Button, "New Module").click();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "New Module").is_some());
    assert!(harness.query_by_label("Name:").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "Create").is_some());
}

#[test]
fn cancel_closes_the_module_form_and_restores_the_empty_state() {
    let mut harness = harness();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "New Module").click();
    harness.step();
    assert!(harness.query_by_label("Name:").is_some());

    harness.get_by_role_and_label(Role::Button, "Cancel").click();
    // Two steps: one to process the click (Cancel lives inside the same
    // render function that already drew "Name:" earlier this frame — see
    // this file's module doc comment), one more to see the effect.
    harness.step();
    harness.step();

    assert!(harness.query_by_label("Name:").is_none());
    assert!(
        harness
            .query_by_label("Select an entry in the tree to view it, or use the toolbar to create a new one.")
            .is_some()
    );
}

#[test]
fn new_requirement_button_opens_the_requirement_form_specifically() {
    let mut harness = harness();
    harness.step();

    harness.get_by_role_and_label(Role::Button, "New Requirement").click();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "New Requirement").is_some());
    assert!(harness.query_by_label("Requirement text:").is_some());
    // Proves the four forms are actually distinct, not one generic form
    // wearing different headings — see README's "Center pane: distinct
    // forms per kind."
    assert!(harness.query_by_label("Result kind:").is_none());
}

#[test]
fn attachments_button_opens_the_dialog() {
    let mut harness = harness();
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Attachments…").click();
    harness.step();

    // The dialog's own heading, "Attachments" (no ellipsis) — distinct
    // from the toolbar button's "Attachments…" label.
    assert!(harness.query_by_role_and_label(Role::Label, "Attachments").is_some());
}

#[test]
fn close_button_closes_the_attachments_dialog() {
    let mut harness = harness();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Attachments…").click();
    harness.step();
    // A newly-opened `egui::Modal` needs one extra frame to "settle"
    // before its own content is reliably clickable — confirmed
    // empirically: without this, the click below on "Close" (which *is*
    // found by the query, and *is* inside the modal) has no effect at
    // all, every time. One `step()` is enough to query the modal's
    // content (as the assert right below shows), but not yet enough to
    // interact with it.
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Label, "Attachments").is_some());

    harness.get_by_role_and_label(Role::Button, "Close").click();
    // Two more steps: one to process the click (Close lives inside the
    // same render function that already drew the heading earlier this
    // frame — see this file's module doc comment on the two-step
    // pattern), one more to see the effect.
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "Attachments").is_none());
}

#[test]
fn opening_a_real_project_populates_the_tree() {
    let mut harness = harness();
    harness.step();
    assert!(harness.query_by_label("No project loaded.").is_some());

    open_sample_project(&mut harness);

    assert!(harness.query_by_label("No project loaded.").is_none());
    // The project's own name, from `sample_project/project.ron` — proves
    // real data came back from a real `LoadProject`, not just that the
    // placeholder message went away.
    assert!(harness.query_by_label("Capstone").is_some());
}

#[test]
fn the_tree_starts_fully_collapsed_when_a_project_first_opens() {
    let mut harness = harness();
    harness.step();

    // Deliberately not `open_sample_project` — that helper clicks
    // "Expand All" right after load for every *other* test's benefit
    // (see its own doc comment). This test exercises the real opened
    // state before any such click, so it drives `open_project` directly.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_project");
    harness.state_mut().open_project(path);
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Capstone").is_some());

    // Every module (e.g. "setup") and every leaf group folder
    // ("requirements"/"tests"/"results") starts collapsed — see
    // `render_tree_node`/`render_leaf_group`'s own `default_open(false)`
    // — so none of their children are reachable yet.
    assert!(harness.query_by_role_and_label(Role::Button, "setup").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "requirements").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "tests").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "results").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} definition").is_none());

    // "Expand All" reveals it, proving the leaf was only hidden by the
    // collapsed header, not missing from the tree entirely.
    harness.get_by_role_and_label(Role::Button, "Expand All").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} definition").is_some());
}

#[test]
fn the_tree_groups_leaves_under_requirements_tests_and_results_folders() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Capstone").is_some());

    // The three folders themselves — root-level, since `sample_project`
    // has root-level requirements/tests/results, not just ones nested in
    // a submodule. A `CollapsingHeader`'s own label reports as
    // `Role::Button` (it's clickable, toggling expand/collapse), same as
    // a module's own name — not `Role::Label`.
    assert!(harness.query_by_role_and_label(Role::Button, "requirements").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "tests").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "results").is_some());

    // `open_project_at` already clicked "Expand All" — a real leaf
    // underneath is visible and clickable. "definition" is a real
    // root-level requirement; its tree label carries the
    // unvalidated-status glyph.
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} definition").is_some());
}

#[test]
fn an_empty_module_shows_no_leaf_group_folders() {
    let harness = dirty_harness();

    // `dirty_harness` creates "interaction_test_module" with no
    // requirements/tests/results in it — `render_leaf_group` omits an
    // empty group entirely (see its own doc comment), so it must not add
    // any new "requirements"/"tests"/"results" folder beyond the three
    // real root-level ones `sample_project` already has. A before/after
    // count would need two snapshots; instead this just pins the count
    // each already has exactly one match — proving the empty module
    // contributed zero, not that some *other* pre-existing count changed.
    assert_eq!(harness.get_all_by_role_and_label(Role::Button, "requirements").count(), 1);
    assert_eq!(harness.get_all_by_role_and_label(Role::Button, "tests").count(), 1);
    assert_eq!(harness.get_all_by_role_and_label(Role::Button, "results").count(), 1);
    assert!(harness.query_by_role_and_label(Role::Button, "interaction_test_module").is_some());
}

#[test]
fn typing_into_the_filter_bar_hides_non_matching_leaves_and_modules() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Capstone").is_some());

    // Both the "definition" requirement and the "definition" result are
    // real root-level leaves in `sample_project` before any filtering.
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} definition").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} discovery").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "setup").is_some());

    // Index 0 is the zoom field (see `zoom_field_value`'s own comment);
    // the filter field is the next `TextInput` in tree order, drawn
    // right after it in the status bar/left pane.
    let filter_field =
        harness.get_all_by_role(Role::TextInput).nth(1).expect("filter field not found");
    filter_field.focus();
    filter_field.type_text("definition");
    harness.step();
    harness.step();

    // The matching leaf survives; a non-matching sibling leaf and every
    // module (none of which has a "definition"-named descendant in
    // `sample_project`) are filtered out.
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} definition").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} discovery").is_none());
    assert!(harness.query_by_role_and_label(Role::Button, "setup").is_none());

    // Clearing the filter (via the "×" button) restores full visibility.
    harness.get_by_role_and_label(Role::Button, "×").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} discovery").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "setup").is_some());
}

/// Opens `sample_project`, creates a module through the real toolbar/form
/// flow, and waits for the resulting `Outcome::AddModule(Ok(()))` to mark
/// the project dirty — the shared setup for every test below that needs a
/// real dirty project to start from.
fn dirty_harness<'a>() -> Harness<'a, GuiApp> {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Module").click();
    harness.step();
    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness.get_all_by_role(Role::TextInput).last().expect("name field not found");
    name_field.focus();
    name_field.type_text("interaction_test_module");
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Create").click();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("\u{e18a} unsaved changes").is_some());
    harness
}

#[test]
fn creating_a_module_in_a_loaded_project_marks_it_dirty() {
    let harness = dirty_harness();

    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
    assert!(harness.query_by_label("saved").is_none());
}

#[test]
fn undo_and_redo_round_trip_a_real_module_creation() {
    let mut harness = dirty_harness();

    // `dirty_harness` already created "interaction_test_module" through
    // the real toolbar/form flow — a real `AddModule` that pushed a real
    // undo snapshot in `gui-core`.
    assert!(harness.query_by_role_and_label(Role::Button, "interaction_test_module").is_some());
    assert!(!harness.get_by_role_and_label(Role::Button, "Undo").accesskit_node().is_disabled());
    assert!(harness.get_by_role_and_label(Role::Button, "Redo").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "Undo").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "interaction_test_module").is_none()
    });

    assert!(!harness.get_by_role_and_label(Role::Button, "Redo").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "Redo").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "interaction_test_module").is_some()
    });
}

#[test]
fn undo_is_disabled_with_no_project_loaded() {
    let mut harness = harness();
    harness.step();

    assert!(harness.get_by_role_and_label(Role::Button, "Undo").accesskit_node().is_disabled());
    assert!(harness.get_by_role_and_label(Role::Button, "Redo").accesskit_node().is_disabled());
}

#[test]
fn exit_with_unsaved_changes_shows_the_confirmation_dialog() {
    let mut harness = dirty_harness();

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("You have unsaved changes. Save before exiting?").is_some());
    // "Discard"/"Cancel" are unique to this dialog; "Save" isn't checked
    // here — the toolbar's own persistent Save button shares that exact
    // role+label, so it's an ambiguous query while both are visible at
    // once (confirmed empirically: `query_by_role_and_label` panics on
    // "found two or more nodes"). The dialog's own message text already
    // confirms it's showing.
    assert!(harness.query_by_role_and_label(Role::Button, "Discard").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "Cancel").is_some());
}

#[test]
fn cancel_on_the_exit_dialog_dismisses_it_and_stays_open() {
    let mut harness = dirty_harness();
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_label("You have unsaved changes. Save before exiting?").is_some());

    harness.get_by_role_and_label(Role::Button, "Cancel").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("You have unsaved changes. Save before exiting?").is_none());
    // Cancelling the exit is not the same as discarding the edit — still
    // dirty, still showing the normal toolbar.
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
    assert!(harness.query_by_role_and_label(Role::Button, "New Module").is_some());
}

#[test]
fn discard_on_the_exit_dialog_closes_it_and_proceeds() {
    let mut harness = dirty_harness();
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_label("You have unsaved changes. Save before exiting?").is_some());

    harness.get_by_role_and_label(Role::Button, "Discard").click();
    harness.step();
    harness.step();

    // Stage 2 (the actual `Command::Shutdown` + viewport close) is
    // exercised exhaustively at the logic level in `src/lib.rs`'s own
    // tests (`take_ready_to_exit`, `discard_proceeds_to_exit_without_saving`);
    // what this proves at the rendering level is that a real click on the
    // real "Discard" button really does resolve the dialog, which is the
    // part those tests can't see.
    assert!(harness.query_by_label("You have unsaved changes. Save before exiting?").is_none());
}

#[test]
fn a_validated_requirements_tree_leaf_shows_the_unmet_status_icon() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // Before validating, every requirement is `EntryStatus::Unvalidated`
    // (see the test above) — `Validate` actually resolves real Met/Unmet
    // status via `logical::validate`. None of `sample_project`'s
    // requirements have a passing `Result` wired up to satisfy them, so
    // every one of them (including "definition") comes back `Unmet` —
    // confirmed empirically, not a fixture property documented elsewhere.
    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());

    // `\u{e4f8}` is `icons::status_icon`'s `X_CIRCLE` (Unmet) — the same
    // icon+color-chip path `render_leaf` now runs for every requirement
    // via `theme_colors::status_colors`, exercised here via a real
    // `EntryStatus::Unmet` rather than the default `Unvalidated` every
    // other test in this file sees.
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e4f8} definition").is_some());
    // The old unvalidated-status icon is gone now that it's actually
    // Unmet.
    assert!(harness.query_by_role_and_label(Role::Button, "\u{e32c} definition").is_none());
}

#[test]
fn the_requirement_viewer_explains_why_it_is_unmet() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());

    // Same real, fact-checked outcome as
    // `a_validated_requirements_tree_leaf_shows_the_unmet_status_icon` —
    // "definition"'s own pinned test reference
    // (`07b53180d4cdcce38d1566e3d2c690b479be1514`, in `requirement.ron`)
    // no longer matches `/tests/generic_inspection`'s real current commit
    // in this repo's own git history, so it's Unmet with a genuine stale-
    // reference reason, not a placeholder.
    harness.get_by_role_and_label(Role::Button, "\u{e4f8} definition").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Requirement").is_some());

    assert!(harness.query_by_label("Status:").is_some());
    assert!(harness.query_by_label("Unmet").is_some());
    assert!(
        harness
            .query_by_label_contains("Test \"/tests/generic_inspection\": its reference is stale")
            .is_some()
    );
}

#[test]
fn clicking_validate_refreshes_an_already_open_requirement_viewer() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // Open "definition"'s viewer *first*, before validating — it starts
    // `Unvalidated` (the project hasn't been validated in this session
    // yet), same starting point `selecting_an_existing_requirement_opens_its_read_only_viewer`
    // documents.
    harness.get_by_role_and_label(Role::Button, "\u{e32c} definition").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Requirement").is_some());
    assert!(harness.query_by_label("Unvalidated").is_some());

    // Validate *without navigating away* — the still-open viewer above is
    // the thing under test, not a freshly reopened one (that path is
    // already covered by `the_requirement_viewer_explains_why_it_is_unmet`).
    // This is the regression test for the gap `GuiApp::apply_outcome`
    // used to have no `Outcome::Validate` arm at all for.
    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());

    // Same real fact the other Unmet-status tests establish: "definition"'s
    // own test reference is genuinely stale against this repo's real git
    // history.
    assert!(harness.query_by_label("Unmet").is_some());
    assert!(harness.query_by_label("Unvalidated").is_none());
    assert!(
        harness
            .query_by_label_contains("Test \"/tests/generic_inspection\": its reference is stale")
            .is_some()
    );
    // Still the same viewer, not bounced back to some other screen.
    assert!(harness.query_by_role_and_label(Role::Label, "Requirement").is_some());
}

#[test]
fn the_update_stale_references_button_appears_only_for_a_stale_reference_and_fixes_it() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // Before validating, nothing is known to be stale yet — the button
    // must not show for an `Unvalidated` requirement.
    harness.get_by_role_and_label(Role::Button, "\u{e32c} definition").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Requirement").is_some());
    assert!(harness.query_by_label_contains("Update Stale References").is_none());

    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());

    // Same real, fact-checked outcome the other Unmet-status tests
    // establish — "definition" is genuinely `Unmet` with a stale
    // `/tests/generic_inspection` reference now.
    assert!(harness.query_by_label("Unmet").is_some());
    // `\u{e094}` is `icons::UPDATE_STALE_REFERENCES` (`ARROWS_CLOCKWISE`) —
    // confirmed via the button's own accessible label, same "concatenated
    // icon + text" shape every other icon-plus-text button here has.
    harness.get_by_role_and_label(Role::Button, "\u{e094} Update Stale References").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());
    // Two more frames for the re-fetched detail's own render to catch up
    // — found empirically (one wasn't enough), same "a click's effect
    // often needs extra settling" timing this file's own module doc
    // describes for other cases; exact cause not fully pinned down here
    // either.
    harness.step();
    harness.step();

    // Fixing it is itself an edit, so it demotes back to `Draft` same as
    // any other — but `RefreshStaleTestReferences` implicitly revalidates
    // on success (see `apply_refresh_stale_test_references_result`'s own
    // doc comment), so by the time it completes the project is already
    // re-`Validated` and the status line reads the real status straight
    // away, with no separate `Validate` call needed. The reference itself
    // is current now, so the remaining reason (if any) can no longer be
    // the stale reference — `sample_project`'s results are all
    // `Incomplete`, not `Pass`, so it's still `Unmet`, just for a
    // different, real reason. The button itself is gone too — nothing
    // (still) known to be stale about the reference now that it's fixed.
    assert!(harness.query_by_label("Unmet").is_some());
    assert!(harness.query_by_label_contains("Update Stale References").is_none());
    // `apply_refresh_stale_test_references_result` marks `dirty` on
    // success, same as any other real edit.
    assert!(harness.query_by_label_contains("unsaved changes").is_some());

    // Re-validate: the reference is current now, so it can no longer be
    // reported as stale — `sample_project`'s results are all `Incomplete`,
    // not `Pass`, so "definition" is still `Unmet`, just for a different,
    // real reason (no current passing result) than before the fix, and
    // the button (nothing left for it to fix) stays gone.
    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());

    assert!(harness.query_by_label("Unmet").is_some());
    assert!(
        harness
            .query_by_label_contains("Test \"/tests/generic_inspection\": its reference is stale")
            .is_none()
    );
    assert!(
        harness
            .query_by_label_contains("no current, passing result exists for it")
            .is_some()
    );
    assert!(harness.query_by_label_contains("Update Stale References").is_none());
}

#[test]
fn selecting_an_existing_requirement_opens_its_read_only_viewer() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // "definition" is a real root-level requirement in `sample_project`
    // (see `disk`'s own tests); its tree label includes the "\u{e32c}"
    // (MINUS_CIRCLE) unvalidated-status icon (`icons::status_icon`) since
    // this project hasn't been validated in this session.
    harness.get_by_role_and_label(Role::Button, "\u{e32c} definition").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Requirement").is_some());

    // The bare "Requirement" heading (not "Edit Requirement"/
    // "New Requirement") is the viewer's own — clicking a tree leaf lands
    // there by default, not straight into the editable form.
    assert!(harness.query_by_role_and_label(Role::Label, "Requirement").is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_none());
    // The real title from `sample_project/requirements/definition/requirement.ron`,
    // shown as plain read-only text — no `Role::TextInput` for it, unlike
    // the editable form (see the next test).
    assert!(harness.query_by_label("Definition").is_some());
    assert!(harness.query_by_role_and_label(Role::TextInput, "Definition").is_none());
    // Only the toolbar's persistent "Save" exists — the form itself has
    // no Save/Cancel row while read-only, only "Edit".
    assert_eq!(harness.get_all_by_role_and_label(Role::Button, "Save").count(), 1);
    assert!(harness.query_by_role_and_label(Role::Button, "Edit").is_some());

    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());

    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
    // `query_by_value` alone is ambiguous here (a `TextInput` node and its
    // own child `TextRun` both carry the same value, unlike labels, which
    // filter out the "labelled-by" node) — `get_all_by_value` sidesteps
    // that by not requiring uniqueness.
    assert!(harness.get_all_by_value("Definition").next().is_some());
    // Two "Save" buttons now exist — the toolbar's persistent one and the
    // form's own (same ambiguity as the exit dialog's "Save" — see that
    // test's comment) — so this checks count, not a single unique query.
    assert_eq!(harness.get_all_by_role_and_label(Role::Button, "Save").count(), 2);
    // Renaming isn't supported once editing — the name field is disabled.
    let name_field = harness.get_all_by_value("definition").next().expect("name field not found");
    assert!(name_field.accesskit_node().is_disabled());
}

#[test]
fn back_and_forward_toolbar_buttons_round_trip_two_real_selections() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    assert!(harness.get_by_role_and_label(Role::Button, "Back").accesskit_node().is_disabled());
    assert!(harness.get_by_role_and_label(Role::Button, "Forward").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "\u{e32c} definition").click();
    harness.step();
    // Lands on the read-only viewer — "Definition" shows as a plain
    // label, not a `TextInput` value, until "Edit" is clicked (see
    // `selecting_an_existing_requirement_opens_its_read_only_viewer`).
    wait_until(&mut harness, |h| h.query_by_label("Definition").is_some());
    // Opening a project lands on its own root page first — that's
    // history stop #0 — so this first real leaf selection already has
    // somewhere to go Back to.
    assert!(!harness.get_by_role_and_label(Role::Button, "Back").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "\u{e32c} discovery").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Discovery").is_some());
    assert!(!harness.get_by_role_and_label(Role::Button, "Back").accesskit_node().is_disabled());
    assert!(harness.get_by_role_and_label(Role::Button, "Forward").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "Back").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Definition").is_some());
    assert!(!harness.get_by_role_and_label(Role::Button, "Forward").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "Forward").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Discovery").is_some());
}

#[test]
fn the_edit_buttons_navigation_registers_with_back_and_forward() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "\u{e32c} definition").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Button, "Edit").is_some());
    // Opening a project lands on its own root page first — that's
    // history stop #0 — so this first real leaf selection already has
    // somewhere to go Back to.
    assert!(!harness.get_by_role_and_label(Role::Button, "Back").accesskit_node().is_disabled());

    // Clicking "Edit" is itself a navigation — per the user's own
    // request, it must register with Back/Forward the same as clicking a
    // different tree leaf does.
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
    assert!(!harness.get_by_role_and_label(Role::Button, "Back").accesskit_node().is_disabled());
    assert!(harness.get_by_role_and_label(Role::Button, "Forward").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "Back").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Requirement").is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_none());
    assert!(!harness.get_by_role_and_label(Role::Button, "Forward").accesskit_node().is_disabled());

    harness.get_by_role_and_label(Role::Button, "Forward").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
}

#[test]
fn saving_an_edit_to_an_existing_requirement_keeps_the_form_open_and_marks_dirty() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");
    assert!(harness.query_by_label("saved").is_some());

    let title_field = harness.get_all_by_value("Definition").next().expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    // The form's own Save button — the second "Save" in tree order (the
    // toolbar's persistent one, rendered earlier in the frame, is first —
    // see the previous test's comment on this same ambiguity). Pinned
    // next to the heading rather than only at the bottom specifically so
    // it stays reachable with a plain `.click()` (a real on-screen
    // position, unlike `.click_accesskit()`) even though this form runs
    // taller than the default 800x600 test viewport (dependencies
    // section included) — see README's "Center pane: distinct forms per
    // kind."
    harness
        .get_all_by_role_and_label(Role::Button, "Save")
        .nth(1)
        .expect("form Save button not found")
        .click();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("\u{e18a} unsaved changes").is_some());

    // The form is still showing the same entry — a successful edit-mode
    // Save leaves it open (per `apply_update_result`, unlike a creation-
    // mode Save, which closes the form), not reset to the empty state.
    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn navigating_away_from_an_edited_field_prompts_and_cancel_stays_put() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");

    let title_field = harness.get_all_by_value("Definition").next().expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    // A different tree leaf — this must not navigate away immediately.
    harness.get_by_role_and_label(Role::Button, "\u{e32c} discovery").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_some());
    // Still on "definition"'s edit form, untouched.
    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
    assert!(harness.get_all_by_value("Definition (edited)").next().is_some());

    // Two "Cancel" buttons exist right now — the still-open edit form's
    // own, and the confirm modal's own, rendered after it (see `ui()`'s
    // render order) and so last in tree order.
    harness
        .get_all_by_role_and_label(Role::Button, "Cancel")
        .last()
        .expect("confirm dialog's Cancel button not found")
        .click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_none());
    // Cancelling the prompt leaves the edit exactly as it was — neither
    // navigated away nor itself discarded (that's what the form's own
    // Cancel button is for, a separate, deliberately unprompted action).
    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
    assert!(harness.get_all_by_value("Definition (edited)").next().is_some());
}

#[test]
fn navigating_away_from_an_edited_field_prompts_and_continue_discards_and_navigates() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");

    let title_field = harness.get_all_by_value("Definition").next().expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "\u{e32c} discovery").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_some());

    harness.get_by_role_and_label(Role::Button, "Continue").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_none());
    // Landed on "discovery"'s own viewer — the click that was interrupted
    // actually went through once confirmed.
    wait_until(&mut harness, |h| h.query_by_label("Discovery").is_some());
    assert!(harness.query_by_label("Discovery").is_some());
}

#[test]
fn navigating_away_from_an_untouched_edit_form_does_not_prompt() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    // Into the editable form, but nothing typed — Edit alone doesn't mark
    // anything `edited`.
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");

    harness.get_by_role_and_label(Role::Button, "\u{e32c} discovery").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_none());
    wait_until(&mut harness, |h| h.query_by_label("Discovery").is_some());
    assert!(harness.query_by_label("Discovery").is_some());
}

#[test]
fn the_forms_own_cancel_button_discards_without_prompting() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");

    let title_field = harness.get_all_by_value("Definition").next().expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    // The form's own Cancel — the second "Cancel" in tree order isn't a
    // concern here (no other "Cancel" exists with a project loaded and
    // no other dialog open), unlike the ambiguous "Save"/toolbar cases
    // elsewhere in this file.
    harness.get_by_role_and_label(Role::Button, "Cancel").click();
    harness.step();
    harness.step();

    // No prompt at all — Cancel is already the explicit "discard" action,
    // see `PendingNavigation`'s own doc comment on why it's excluded from
    // this gate.
    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_none());
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Requirement").is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_none());
}

#[test]
fn back_and_forward_are_gated_on_unsaved_form_edits_too() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "\u{e32c} definition").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Definition").is_some());
    harness.get_by_role_and_label(Role::Button, "\u{e32c} discovery").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Discovery").is_some());

    // Now edit "discovery", then try Back — it should prompt rather than
    // silently navigating away from the unsaved edit. Nav history at
    // this point: definition(View), discovery(View), discovery(Edit) —
    // Back moves one step, to discovery(View), not all the way back to
    // definition (see "Forwards/backwards navigation" — clicking Edit is
    // its own navigation step).
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
    let title_field = harness.get_all_by_value("Discovery").next().expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Back").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_some());

    harness.get_by_role_and_label(Role::Button, "Continue").click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Requirement").is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "Edit Requirement").is_none());
    assert!(harness.get_all_by_value("Discovery").next().is_some());
}

#[test]
fn new_requirement_button_is_gated_on_unsaved_form_edits() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");

    let title_field = harness.get_all_by_value("Definition").next().expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "New Requirement").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_label("This form has unsaved changes. Continue and lose them?").is_some());
    // Still editing "definition" — the click didn't go through yet.
    assert!(harness.get_all_by_value("Definition (edited)").next().is_some());

    harness.get_by_role_and_label(Role::Button, "Continue").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "New Requirement").is_some());
}

#[test]
fn a_requirements_dependency_can_be_viewed_removed_and_a_new_one_added() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // Viewer first — "definition" has one real dependency in
    // `sample_project` (on "discovery", see `requirement.ron`); its
    // summary text (`DependencyDraft`'s own `Display` impl) should be
    // visible as plain read-only text, no `TextInput`/Remove button.
    harness.get_by_role_and_label(Role::Button, "\u{e32c} definition").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label("requirements/discovery @ 07b53180d4cdcce38d1566e3d2c690b479be1514").is_some()
    });
    assert!(harness.query_by_role_and_label(Role::Button, "Remove").is_none());

    // Into the editable form — the same dependency is now editable, with
    // a "Remove" button (and no local attachments on "definition" to add
    // a second one — see `adding_a_local_attachment_...`'s own comment on
    // this same fixture entry).
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Label, "Edit Requirement").is_some());
    assert_eq!(harness.get_all_by_role_and_label(Role::Button, "Remove").count(), 1);

    // Add a new dependency via the composer — the default `Local`
    // variant's path/commit fields are the *first* two `TextInput`s among
    // the dependency-related ones once the existing one is gone (see
    // `adding_a_local_attachment_...`'s own field-order comment; removing
    // the existing dependency below drops its two fields entirely).
    harness
        .get_all_by_role_and_label(Role::Button, "Remove")
        .next()
        .expect("existing dependency's Remove button not found")
        .click_accesskit();
    harness.step();
    harness.step();
    // `query_all_by_*`, not `get_all_by_*` — the latter panics on zero
    // matches (it's the "get" semantics: assert something's there), which
    // is exactly the case being asserted here.
    assert_eq!(harness.query_all_by_role_and_label(Role::Button, "Remove").count(), 0);

    // name(2), title(3), then the composer's own path(4)/commit(5) —
    // the existing dependency's two fields are gone now that it's been
    // removed.
    harness
        .get_all_by_role(Role::TextInput)
        .nth(4)
        .expect("dependency composer path field not found")
        .focus();
    harness
        .get_all_by_role(Role::TextInput)
        .nth(4)
        .unwrap()
        .type_text("/requirements/implementation");
    harness.step();
    harness
        .get_all_by_role(Role::TextInput)
        .nth(5)
        .expect("dependency composer commit field not found")
        .focus();
    harness.get_all_by_role(Role::TextInput).nth(5).unwrap().type_text("newcommit");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Add dependency").click_accesskit();
    harness.step();
    harness.step();

    // Pushed onto the (now-editable, still edit-mode) list — its own
    // Remove button confirms a real new row exists, and its value is
    // still typed-in plain fields at this point, not the read-only
    // summary string (that only renders once `read_only` — see the
    // viewer assertion below, after Save returns to it).
    assert_eq!(harness.query_all_by_role_and_label(Role::Button, "Remove").count(), 1);
    assert!(harness.get_all_by_value("/requirements/implementation").next().is_some());
    assert!(harness.get_all_by_value("newcommit").next().is_some());

    // Plain `.click()`, not `.click_accesskit()` — Save is pinned next
    // to the heading now (see the earlier Save test's own comment), so
    // it's reachable at a real on-screen position even in a form this
    // long (an existing dependency plus a newly-added one, both with
    // their own fields).
    harness
        .get_all_by_role_and_label(Role::Button, "Save")
        .nth(1)
        .expect("form Save button not found")
        .click();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("\u{e18a} unsaved changes").is_some());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn an_existing_dependencys_own_pick_and_auto_buttons_update_that_row() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");

    // "definition" has one real dependency in `sample_project` (on
    // "discovery", commit `07b53180d4cdcce38d1566e3d2c690b479be1514` —
    // see `a_requirements_dependency_can_be_viewed_removed_and_a_new_one_added`).
    // Every other Pick/Auto test here only ever exercises the "Add
    // dependency" composer's own row (`DependencySlot::New`) — this one
    // targets the *existing* row instead (`DependencySlot::Existing(0)`),
    // a genuinely different code path in both `path_picker_dialog_selected`
    // and `dependency_commit_auto_clicked`. Two "Pick…"/"Auto" buttons
    // exist now (the existing row's, then the composer's own) — `.next()`
    // reaches the existing row's in both cases.
    let initial_commit = harness
        .get_all_by_role(Role::TextInput)
        .nth(5)
        .and_then(|field| field.value())
        .expect("existing dependency's commit field not found");
    assert_eq!(initial_commit, "07b53180d4cdcce38d1566e3d2c690b479be1514");

    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("existing dependency's Pick button not found")
        .click();
    harness.step();
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Label, "Pick a requirement").is_some());

    // "implementation" is a different real root-level requirement —
    // switching the existing row to point at it (rather than re-picking
    // "discovery") makes the follow-up Auto fetch below meaningfully
    // check something changed, not just that the field still happened to
    // hold a hex string.
    harness.get_all_by_label("implementation").last().expect("modal row not found").click();
    harness.step();
    harness.step();

    assert!(harness.get_all_by_value("/requirements/implementation").next().is_some());
    // Still the *existing* row that changed, not a second row added —
    // exactly one "Remove" button (dependencies) still present.
    assert_eq!(harness.get_all_by_role_and_label(Role::Button, "Remove").count(), 1);

    harness
        .get_all_by_role_and_label(Role::Button, "Auto")
        .next()
        .expect("existing dependency's Auto button not found")
        .click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.get_all_by_role(Role::TextInput)
            .nth(5)
            .and_then(|field| field.value())
            .is_some_and(|value| value != initial_commit && value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()))
    });
}

#[test]
fn requirement_form_dependency_path_picker_fills_the_field() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Requirement").click();
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Label, "New Requirement").is_some());

    // The "Add dependency" composer's default `Local` variant carries its
    // own path picker now — same modal mechanics as the Result form's own
    // pickers (see `result_form_requirement_path_picker_fills_the_field`).
    // Exactly one "Pick…" button exists in a fresh create-mode form (no
    // existing dependencies to add their own).
    harness.get_by_role_and_label(Role::Button, "Pick…").click();
    harness.step();
    harness.step(); // let the modal settle, same as the Result form's own test.

    assert!(harness.query_by_role_and_label(Role::Label, "Pick a requirement").is_some());

    // "discovery" is a real root-level requirement in `sample_project`.
    // As with the Result form's own pickers, the tree's own leaf button
    // for it also matches by label and sorts first in tree order, so
    // `.last()` is what actually reaches the modal's own row.
    harness.get_all_by_label("discovery").last().expect("modal row not found").click();
    harness.step();
    harness.step();

    assert!(harness.get_all_by_value("/requirements/discovery").next().is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "New Requirement").is_some());
}

#[test]
fn requirement_form_dependency_auto_button_fetches_the_commit() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Requirement").click();
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Pick…").click();
    harness.step();
    harness.step();
    harness.get_all_by_label("discovery").last().expect("modal row not found").click();
    harness.step();
    harness.step();

    // Exactly one "Auto" button exists here too — the composer's default
    // `Local` variant has one commit field, so one "Auto" button next to
    // it. Clicking it round-trips through the real `CoreHandle`'s actor,
    // which shells out to real `git` against `sample_project` (a real,
    // tracked directory in this very repo — see `open_sample_project`'s
    // own doc comment) to resolve `requirements/discovery`'s latest
    // commit.
    harness.get_by_role_and_label(Role::Button, "Auto").click();
    harness.step();

    // The commit field is the composer's own second `Role::TextInput` —
    // name(2), title(3), then path(4)/commit(5), same indexing
    // `adding_a_local_attachment_to_an_existing_requirement_appears_in_the_list`'s
    // own comment documents (there are no existing dependencies here to
    // shift the composer's fields further down). Asserted by shape (40
    // hex characters — a real commit hash), not a specific value, since
    // the actual commit depends on this repo's own history rather than
    // anything `sample_project`'s fixture data pins in place.
    wait_until(&mut harness, |h| {
        h.get_all_by_role(Role::TextInput)
            .nth(5)
            .and_then(|field| field.value())
            .is_some_and(|value| value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()))
    });
}

#[test]
fn requirement_form_remote_dependency_auto_button_fetches_the_commit() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Requirement").click();
    harness.step();

    // Switches the "Add dependency" composer from its default `Local`
    // variant to `Remote` — a real, separate code path in
    // `render_dependency_fields`/`dependency_commit_auto_clicked`
    // (`AutoCommitKind::Remote`, resolved via `RemoteGit::commit_for_remote`
    // rather than `Git::commit_for_path`), untested until now (the
    // existing Auto tests only ever exercise the `Local` variant).
    harness.get_by_role_and_label(Role::RadioButton, "Remote").click();
    harness.step();

    // name(2), title(3), then the composer's own URL(4)/Path(5)/Commit(6)
    // — same indexing convention as the `Local` variant's test, just with
    // `Remote`'s three fields instead of two.
    let url_field = harness.get_all_by_role(Role::TextInput).nth(4).expect("url field not found");
    url_field.focus();
    // This repo's own root, addressed as a `file://` remote — a real git
    // repository `commit_for_remote` can actually clone/inspect without
    // any network access, same trick `syscalls`' own tests use (see
    // `file_url` below).
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    url_field.type_text(&file_url(&repo_root));
    harness.step();

    let path_field = harness.get_all_by_role(Role::TextInput).nth(5).expect("path field not found");
    path_field.focus();
    path_field.type_text("sample_project/requirements/discovery");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Auto").click();
    harness.step();

    // Same "assert by shape, not by exact value" reasoning as the `Local`
    // variant's own test — the real commit depends on this repo's own
    // history.
    wait_until(&mut harness, |h| {
        h.get_all_by_role(Role::TextInput)
            .nth(6)
            .and_then(|field| field.value())
            .is_some_and(|value| value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()))
    });
}

#[test]
fn path_picker_dialog_cancel_closes_it_without_changing_the_field() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Result").click();
    harness.step();
    harness.get_all_by_role_and_label(Role::Button, "Pick…").next().expect("requirement path picker not found").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Label, "Pick a requirement").is_some());

    // Not `get_by_role_and_label` — the Result form has its own "Cancel"
    // button too (next to Create), so this is ambiguous under an exact
    // match. The modal renders last in `ui()` (see `render_path_picker_dialog`'s
    // own doc comment), so its own Cancel button is reliably the *last*
    // match in tree order.
    harness
        .get_all_by_role_and_label(Role::Button, "Cancel")
        .last()
        .expect("picker's own Cancel button not found")
        .click();
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "Pick a requirement").is_none());
    // The requirement-path field is untouched — still empty, not filled
    // in by a cancelled picker. `query_by_value`, not `get_all_by_value`
    // (which panics on zero matches — exactly the case being asserted).
    assert!(harness.query_by_value("/requirements/definition").is_none());
    assert!(harness.query_by_role_and_label(Role::Label, "New Result").is_some());
}

#[test]
fn path_picker_dialog_shows_no_matches_for_an_unmatched_filter() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Result").click();
    harness.step();
    harness.get_all_by_role_and_label(Role::Button, "Pick…").next().expect("requirement path picker not found").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("No matches.").is_none());

    // No requirement in `sample_project` has "zzz" anywhere in its
    // fully-qualified path, so this should empty the list out entirely —
    // `render_path_picker_dialog`'s own fallback text for that case.
    let filter_field = harness.get_all_by_role(Role::TextInput).last().expect("filter field not found");
    filter_field.focus();
    filter_field.type_text("zzz_no_such_entry");
    harness.step();

    assert!(harness.query_by_label("No matches.").is_some());
    // Confirms the list itself is actually empty, not just coincidentally
    // missing that one label — the modal's own row for "definition" is
    // gone (only the tree's own leaf button for it can still match).
    assert_eq!(harness.get_all_by_label("definition").count(), 1);
}

#[test]
fn adding_a_local_attachment_to_an_existing_requirement_appears_in_the_list() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} definition", "Edit Requirement");

    // None of the requirement form's text fields are accessibility-
    // labelled (just a preceding `ui.label`, never wired via
    // `.labelled_by`), so they're only distinguishable by role + tree
    // order. `ui.text_edit_multiline` (requirement text/guidance, test
    // guidance) reports as `Role::MultilineTextInput`, a *different* role
    // from `ui.text_edit_singleline`'s `Role::TextInput` — found
    // empirically after `Role::TextInput` alone turned up only 3 fields,
    // not the expected 5. So among `Role::TextInput` nodes specifically,
    // the order is: the status bar's own zoom field(0) and the left
    // pane's own filter field(1) — both always first — then name(2),
    // title(3), then — "definition" has one real dependency in
    // `sample_project` (on "discovery") — its own path(4)/commit(5)
    // fields, then the "Add dependency" composer's own default `Local`
    // path(6)/commit(7) fields (always present, even with zero
    // dependencies to add), then — since this form is in edit mode — the
    // local-attachment path field(8). Fragile to reordering singleline
    // fields specifically, which is why this comment exists.
    let attachment_path_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(8)
        .expect("local-attachment path field not found");
    attachment_path_field.focus();
    attachment_path_field.type_text("interaction_test_attachment.md");
    harness.step();

    // "Add", not "Add dependency" — the two are ambiguous under a plain
    // substring match, but `get_by_role_and_label` requires an exact
    // match, so this alone doesn't collide with the dependency
    // composer's own "Add dependency" button. Both may be off-screen
    // inside the center pane's own `ScrollArea` now that the
    // dependencies section is always present — `.click_accesskit()`
    // sidesteps needing it in view first, same reasoning as this file's
    // other use of it (see the previous test's comment).
    harness.get_by_role_and_label(Role::Button, "Add").click_accesskit();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("interaction_test_attachment.md").is_some());

    assert!(harness.query_by_label("interaction_test_attachment.md").is_some());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn opening_a_project_defaults_to_the_root_view_page() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    assert!(harness.query_by_label("Project: Capstone").is_some());
    wait_until(&mut harness, |h| h.query_by_label("Requirements: 5").is_some());
    assert!(harness.query_by_label("Submodules: 5").is_some());
    assert!(harness.query_by_label("Tests: 5").is_some());
    assert!(harness.query_by_label("Results: 5").is_some());
    assert!(harness.query_by_label("Project not validated — met/pass/fail statistics unavailable.").is_some());
}

/// Also covers the root page's pass/fail stats updating live once
/// Validate completes, per the same button — see `sample_project`'s own
/// "definition" requirement (`the_requirement_viewer_explains_why_it_is_unmet`)
/// for why not every requirement in it validates clean.
#[test]
fn validating_updates_the_root_pages_stats_in_place() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Requirements: 5").is_some());
    assert!(harness.query_by_label("Project not validated — met/pass/fail statistics unavailable.").is_some());

    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_label("Project not validated — met/pass/fail statistics unavailable.").is_none()
    });
    // The results breakdown only ever renders once `summary.validated` is
    // true — its presence alone proves this is a real refresh of the
    // still-open root page (not, say, a coincidentally similar new one).
    assert!(harness.query_by_label("Incomplete: 5 (100%)").is_some());
}

/// Covers both the module view page (summary counts, no rename support
/// needed to see them) and its Edit page — the tree-view rename modal this
/// replaced (`rename_module_dialog_renames_a_real_module`, since removed)
/// used to test the same underlying `RenameModule` round trip through a
/// different, now-deleted UI; this is its replacement.
#[test]
fn module_page_shows_summary_then_renames_a_real_module() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // `sample_project`'s submodules (collection/embeddings/preparation/
    // setup/ui — `ModuleDraft::modules` is a `BTreeMap`, so the tree
    // renders them in sorted order) are all "not current" right after
    // load (the project root is current by default), so `.last()`
    // reliably means "ui"'s own glyph button — see `icons::MODULE_NOT_CURRENT`
    // (Phosphor's `FOLDER_NOTCH`). No other `Role::Button` shares this
    // glyph.
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .last()
        .expect("no module buttons found")
        .click();
    harness.step();

    assert!(harness.query_by_label("Module: ui").is_some());
    // `GetModuleSummary`'s reply goes through the real background actor —
    // wait for it rather than assuming it's already landed after one step.
    wait_until(&mut harness, |h| h.query_by_label("Requirements: 0").is_some());
    assert!(harness.query_by_label("Project not validated — met/pass/fail statistics unavailable.").is_some());

    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_all_by_value("ui").next().is_some());

    let name_field = harness.get_all_by_value("ui").next().expect("name field not found");
    name_field.focus();
    name_field.type_text("_renamed");
    harness.step();
    assert!(harness.get_all_by_value("ui_renamed").next().is_some(), "name field did not become ui_renamed");

    // Two "Save" buttons exist at once — the toolbar's own (always
    // present) and this page's — same ambiguity the requirement/test/
    // result edit forms' own Save already has elsewhere in this file;
    // `.nth(1)` is the page's.
    harness
        .get_all_by_role_and_label(Role::Button, "Save")
        .nth(1)
        .expect("module page Save button not found")
        .click();
    harness.step();

    // Wait for the *tree's* own label to pick up the new name — unlike
    // the still-open text field's value (already "ui_renamed" the moment
    // it was typed, whether or not Save has actually completed yet), the
    // tree only updates once `RenameModule` really lands and pushes a
    // fresh `TreeChanged`. Confirms the round trip end to end; the page's
    // own heading text is already covered at the unit level (see
    // `a_successful_module_rename_updates_the_path_and_returns_to_the_view`
    // in `lib.rs`), so it isn't re-asserted here too.
    wait_until(&mut harness, |h| h.query_by_role_and_label(Role::Button, "Edit").is_some());
    assert!(harness.query_by_label("ui_renamed").is_some());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn result_form_requirement_path_picker_fills_the_field() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Result").click();
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Label, "New Result").is_some());

    // Two "Pick…" buttons exist in this form (requirement path, then test
    // path) — the first in tree order is the requirement-path one. Opens
    // the shared path-picker modal (`render_path_picker_dialog`), which
    // replaced the old per-field `ComboBox` (see that function's own doc
    // comment on why: a `ComboBox` popup doesn't scroll, so it can't
    // handle a project with enough requirements to overflow the screen).
    harness.get_all_by_role_and_label(Role::Button, "Pick…").next().expect("requirement path picker not found").click();
    harness.step();
    harness.step(); // let the modal settle — see this file's module doc.

    assert!(harness.query_by_role_and_label(Role::Label, "Pick a requirement").is_some());

    // "definition" is a real root-level requirement in `sample_project`;
    // the modal row's text is `LogicalPath`'s own `Display` (bare
    // "definition" for a root-level entry — no "modules/..." prefix,
    // unlike the tree's own leaf button, which additionally prefixes a
    // status glyph — "\u{e32c} definition" — so the two don't collide). Two
    // matching "definition" nodes turn up under `get_all_by_label` rather
    // than one, though: the *first* is the tree's own leaf button (its
    // accessibility label is apparently derived without the glyph prefix,
    // despite its visible text being "\u{e32c} definition" — confirmed
    // empirically: clicking it navigates to "Edit Requirement" instead of
    // filling this form's field), and the *second* is the actual modal
    // row, drawn later (the modal renders last in `ui()`) and so later in
    // tree order. `.last()` picks the real row; `.next()`/`.first()` would
    // silently exercise the wrong widget.
    harness.get_all_by_label("definition").last().expect("modal row not found").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "Pick a requirement").is_none());
    assert!(harness.get_all_by_value("/requirements/definition").next().is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "New Result").is_some());
}

#[test]
fn result_form_test_path_picker_fills_the_field() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Result").click();
    harness.step();

    // Same modal mechanics as the requirement-path picker above — two
    // "Pick…" buttons exist in this form; the test-path one is second in
    // tree order.
    harness.get_all_by_role_and_label(Role::Button, "Pick…").nth(1).expect("test path picker not found").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "Pick a test").is_some());

    // "generic_test" is a real root-level test in `sample_project`. As
    // with "definition" above, the tree's own leaf button for it also
    // matches by label and sorts first in tree order, so `.last()` is what
    // actually reaches the modal's own row.
    harness.get_all_by_label("generic_test").last().expect("modal row not found").click();
    harness.step();
    harness.step();

    assert!(harness.get_all_by_value("/tests/generic_test").next().is_some());
    assert!(harness.query_by_role_and_label(Role::Label, "New Result").is_some());
}

#[test]
fn path_picker_dialogs_filter_field_narrows_the_list() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    harness.get_by_role_and_label(Role::Button, "New Result").click();
    harness.step();
    harness.get_all_by_role_and_label(Role::Button, "Pick…").next().expect("requirement path picker not found").click();
    harness.step();
    harness.step();

    // `sample_project` has (at least) two root-level requirements —
    // "definition" and "discovery" — so both show unfiltered, each with
    // two matches by label (the tree's own leaf button, plus the modal's
    // own row — see the previous test's own comment on why every leaf
    // name matches twice).
    assert_eq!(harness.get_all_by_label("definition").count(), 2);
    assert_eq!(harness.get_all_by_label("discovery").count(), 2);

    // The modal's own filter field is its one `Role::TextInput` (the
    // dialog carries no other text field) — narrowing it to "disc" should
    // drop the modal's own "definition" row, leaving only the tree's own
    // leaf button still matching (one match, not two), while "discovery"
    // keeps both (its modal row still matches, on top of its own tree
    // leaf).
    let filter_field = harness.get_all_by_role(Role::TextInput).last().expect("filter field not found");
    filter_field.focus();
    filter_field.type_text("disc");
    harness.step();

    assert_eq!(harness.get_all_by_label("definition").count(), 1);
    assert_eq!(harness.get_all_by_label("discovery").count(), 2);
}

#[test]
fn editing_an_existing_result_can_add_a_local_attachment() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // Unlike a requirement leaf, a result's tree label carries no status
    // glyph prefix (see `render_tree_node` — only `EntryKind::Requirement`
    // gets one), so the button's label is the bare name — unambiguous
    // here even though `sample_project` also has a requirement named
    // "definition" (a real gui-core bug this test caught: `select` used
    // to send `GetEntryDetail` with no `kind`, and the core resolved by
    // trying requirement/test/result pools in a fixed order, so clicking
    // this exact result silently opened the *requirement* named
    // "definition" instead — fixed by having `select`/`render_tree_node`
    // pass the clicked node's own `EntryKind` through).
    open_leaf_for_editing(&mut harness, "definition", "Edit Result");

    // Field order among `Role::TextInput` nodes in edit mode: the status
    // bar's own zoom field(0) and the left pane's own filter field(1) —
    // both always first — then name(2), title(3), requirement_path(4),
    // requirement_commit(5), test_path(6), test_commit(7), then — since
    // this form is editing an existing entry — the local-attachment path
    // field(8). Same "TextInput vs. MultilineTextInput" and tree-order
    // reasoning as the Requirement form's own attachment test.
    let attachment_path_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(8)
        .expect("local-attachment path field not found");
    attachment_path_field.focus();
    attachment_path_field.type_text("interaction_test_result_attachment.md");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Add").click();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("interaction_test_result_attachment.md").is_some());

    assert!(harness.query_by_label("interaction_test_result_attachment.md").is_some());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn editing_an_existing_test_can_add_a_local_attachment_and_template_file() {
    let mut harness = harness();
    harness.step();
    open_sample_project(&mut harness);

    // Same bare-name reasoning as the result leaf above — only
    // requirements carry a status glyph.
    open_leaf_for_editing(&mut harness, "generic_test", "Edit Test");

    // Field order among `Role::TextInput` nodes in edit mode: the status
    // bar's own zoom field(0) and the left pane's own filter field(1) —
    // both always first — then name(2), title(3) (`Result kind:` is a
    // pair of radio buttons, not a text field, so it doesn't count),
    // then — editing an existing entry — the local-attachment path
    // field(4), then the local-template path field(5).
    let attachment_path_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(4)
        .expect("local-attachment path field not found");
    attachment_path_field.focus();
    attachment_path_field.type_text("interaction_test_test_attachment.md");
    harness.step();

    harness
        .get_all_by_role_and_label(Role::Button, "Add")
        .next()
        .expect("attachment Add button not found")
        .click();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("interaction_test_test_attachment.md").is_some());
    assert!(harness.query_by_label("interaction_test_test_attachment.md").is_some());

    let template_path_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(5)
        .expect("local-template path field not found");
    template_path_field.focus();
    template_path_field.type_text("interaction_test_template.md");
    harness.step();

    harness
        .get_all_by_role_and_label(Role::Button, "Add")
        .nth(1)
        .expect("template Add button not found")
        .click();
    harness.step();

    wait_until(&mut harness, |h| h.query_by_label("interaction_test_template.md").is_some());
    assert!(harness.query_by_label("interaction_test_template.md").is_some());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn exit_dialog_saving_then_timeout_lets_the_user_exit_anyway_or_keep_waiting() {
    // `syscalls::SlowFilesystem` makes the real background `Save` take
    // deliberately longer than `save_on_exit_timeout` — a plain
    // `CoreHandle::start()` couldn't reach `TimedOut` deterministically
    // (tried, and reverted: a real `Save` against `sample_project` is
    // pure local disk I/O, sometimes faster than `egui_kittest`'s own
    // `step()` call, so it often won the race to `Ready` before
    // `TimedOut` was ever observable). `SlowFilesystem` only delays
    // `write`/`create_dir_all`, not reads, so `LoadProject` and the
    // module creation below stay fast — only the `Save` this test
    // actually exercises is slow.
    //
    // This is also the one test in this file that completes a real
    // `Save`, so — unlike every other test here — it must run against
    // `scratch_copy_of_sample_project`'s writable copy, not the real
    // fixture: an earlier version of this test pointed `SlowFilesystem`
    // at the real `sample_project` directly, and a real `Save` reaching
    // disk permanently wrote a new module and reformatted every `.ron`
    // file into the repository's own working tree.
    let project_dir = scratch_copy_of_sample_project("exit-dialog-saving");

    let core = gui_core::CoreHandle::start_with(
        syscalls::SlowFilesystem::new(syscalls::StdFilesystem, Duration::from_millis(5)),
        FixedGit,
    );
    let config = GuiConfig {
        save_on_exit_timeout: Duration::from_millis(1),
        ..GuiConfig::default()
    };
    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            core,
            config,
            PathBuf::from("/dev/null"),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    });
    harness.step();
    open_project_at(&mut harness, &project_dir);

    harness.get_by_role_and_label(Role::Button, "New Module").click();
    harness.step();
    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness.get_all_by_role(Role::TextInput).last().expect("name field not found");
    name_field.focus();
    name_field.type_text("interaction_test_module");
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Create").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("\u{e18a} unsaved changes").is_some());

    // `Save` only actually touches the filesystem (and so only actually
    // exercises `SlowFilesystem`'s delay) against a `Validated` project —
    // against a `Draft` it fails immediately with `SaveError::NotValidated`,
    // with no real I/O at all, resolving `Saving` -> `Ready` almost
    // instantly regardless of `SlowFilesystem`. `sample_project` plus an
    // empty new module validates cleanly.
    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("pending").is_none());

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_label("You have unsaved changes. Save before exiting?").is_some());

    // The dialog's own "Save" is the second in tree order — the
    // toolbar's persistent one, rendered earlier in the frame, is first
    // (same ambiguity as every other exit-dialog test's comment on this).
    harness
        .get_all_by_role_and_label(Role::Button, "Save")
        .last()
        .expect("dialog Save button not found")
        .click();
    harness.step();

    // The real `Save` is now sleeping inside its first slow `write`/
    // `create_dir_all` call (at least 5ms away from completing), and
    // the exit dialog's own deadline is only 1ms out — `TimedOut` is
    // reached and observable well before the real save can finish.
    wait_until(&mut harness, |h| {
        h.query_by_label("Still saving — exit anyway and lose unsaved changes, or keep waiting?").is_some()
    });

    harness.get_by_role_and_label(Role::Button, "Keep waiting").click();
    harness.step();
    // Re-arms `Saving` with a fresh 1ms deadline — the real save is
    // still going (its own delay comfortably outlasts a handful of
    // these round trips), so this reaches `TimedOut` again rather than
    // resolving straight to `Ready`. Proves "Keep waiting" genuinely
    // re-arms `Saving` rather than just leaving the same `TimedOut`
    // state on screen.
    wait_until(&mut harness, |h| {
        h.query_by_label("Still saving — exit anyway and lose unsaved changes, or keep waiting?").is_some()
    });

    harness.get_by_role_and_label(Role::Button, "Exit anyway").click();
    // Two steps: one processes the click (setting `Ready`), the next
    // runs `take_ready_to_exit` (which consumes it, sends
    // `Command::Shutdown`, and closes the viewport) at the top of `ui()`
    // before rendering — same "effect needs a second step" pattern as
    // Discard, see `discard_on_the_exit_dialog_closes_it_and_proceeds`.
    harness.step();
    harness.step();

    assert!(harness.query_by_label("Still saving — exit anyway and lose unsaved changes, or keep waiting?").is_none());

    // "Exit anyway" only resolves the *dialog* — the real background
    // `Save` this test deliberately made slow is still mid-write when
    // that happens (`SlowFilesystem`'s 5ms-per-call delay, times roughly a
    // hundred files/dirs in `sample_project`, comfortably
    // outlasts a couple of dialog round trips) and keeps running
    // independently of it. Dropping `harness` (and so the `CoreHandle`/
    // `tokio::runtime::Runtime` it owns) blocks until that in-flight
    // `spawn_blocking` task actually finishes, so the scratch directory
    // is safe to remove afterward — without this, `remove_dir_all` races
    // the still-running save and silently leaves the directory behind.
    drop(harness);
    std::fs::remove_dir_all(&project_dir).ok();
}

#[cfg(all(feature = "debug-panel", debug_assertions))]
#[test]
fn debug_panel_opens_only_after_confirming_and_closes_without_reconfirming() {
    let mut harness = harness();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "Debug").is_none());

    harness.get_by_role_and_label(Role::Button, "Debug").click();
    harness.step();
    harness.step(); // let the modal settle — see this file's module doc.
    assert!(harness.query_by_role_and_label(Role::Label, "Open the debug panel?").is_some());

    harness.get_by_role_and_label(Role::Button, "Cancel").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Label, "Open the debug panel?").is_none());
    assert!(harness.query_by_role_and_label(Role::Label, "Debug").is_none());

    harness.get_by_role_and_label(Role::Button, "Debug").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Open").click();
    harness.step();
    harness.step();

    assert!(harness.query_by_role_and_label(Role::Label, "Debug").is_some());
    // The button's own accessible name stays "Debug" in both states — only
    // its icon (`icons::DEBUG_PANEL_OPEN`/`DEBUG_PANEL_CLOSED`) reflects
    // open vs. closed, so this just re-finds the same button by role+label.
    assert!(harness.query_by_role_and_label(Role::Button, "Debug").is_some());

    // Clicking the same toggle again, now that the panel is open, closes
    // it directly — no confirmation needed a second time (only opening
    // it does), see `GuiApp::debug_panel_button_clicked`'s own comment.
    harness.get_by_role_and_label(Role::Button, "Debug").click();
    harness.step();
    harness.step();
    assert!(harness.query_by_role_and_label(Role::Label, "Debug").is_none());
}

#[cfg(all(feature = "debug-panel", debug_assertions))]
#[test]
fn debug_panel_logs_real_commands_and_can_trigger_a_tx_stall() {
    let mut harness = harness();
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Debug").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Open").click();
    harness.step();
    harness.step();

    // A real toolbar click sends a real `Command::Validate` — it should
    // show up in the log via `GuiApp::send_command`'s interception,
    // proving the log isn't just decorative. `query_by_label_contains`
    // alone is ambiguous (the toolbar's own "Validate" button also
    // "contains" that text), so this filters to `Role::Label`
    // specifically — the log entries' own role, not the button's.
    harness.get_by_role_and_label(Role::Button, "Validate").click();
    harness.step();
    assert!(
        harness
            .get_all_by_role(Role::Label)
            .any(|node| node.accesskit_node().value().is_some_and(|value| value.contains("Validate")))
    );

    harness.get_by_role_and_label(Role::Button, "Tx Stall").click();
    harness.step();
    assert!(harness.query_by_label_contains("Tx is currently stalled").is_some());
}
