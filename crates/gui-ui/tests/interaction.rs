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

use accesskit::{Role, Toggled};
use egui_kittest::Harness;
use egui_kittest::kittest::{By, NodeT as _, Queryable as _};
use gui_ui::{GuiApp, GuiConfig, RecentProjects};

fn harness<'a>() -> Harness<'a, GuiApp> {
    // `/dev/null`: no test built on this helper exercises zoom
    // persistence — see `config::test` in `src/config.rs` for that.
    //
    // `with_size`: `Harness::new_eframe`'s default 800x600 is too short
    // for the tree pane's bottom `ScrollArea` once its leaf groups (and,
    // after "Expand All", the top module tree too) have real content —
    // rows past the fold still get real AccessKit rects/positions beyond
    // the viewport, and `Node::click()` sends a real synthetic pointer
    // event at that position, which is silently a no-op once it's
    // outside `screen_rect`. A generously tall window sidesteps that for
    // every test using this helper rather than each test having to
    // scroll before interacting.
    Harness::builder()
        .with_size(egui::Vec2::new(800.0, 2000.0))
        .build_eframe(|_cc| {
            GuiApp::new(
                gui_core::CoreHandle::start().expect("test tokio runtime"),
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
///
/// 500 attempts (5s worst case) rather than a tighter budget: observed in
/// practice to still time out occasionally at 200 attempts (2s) under the
/// CPU contention of a full `cargo test --workspace` run sharing the
/// machine with other work — widening it only costs real time on the
/// genuinely-slow-under-contention path (the common case still returns as
/// soon as `condition` is met), while still catching an actual hang well
/// short of anything a human would wait out.
fn wait_until(
    harness: &mut Harness<GuiApp>,
    mut condition: impl FnMut(&mut Harness<GuiApp>) -> bool,
) {
    for _ in 0..500 {
        if condition(harness) {
            return;
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition was never met within the step/wait budget");
}

/// Whether a leaf-group `CollapsingHeader` button titled `title` (e.g.
/// "requirements") is present. Its label now carries a trailing count —
/// `"requirements (3)"` or `"requirements (3 · 12 total)"` — so an exact
/// `query_by_role_and_label` no longer matches; `label_contains` finds it
/// regardless of the count suffix.
fn leaf_group_button_present(harness: &Harness<GuiApp>, title: &str) -> bool {
    harness
        .query(By::new().role(Role::Button).label_contains(title))
        .is_some()
}

/// A `file://` remote URL for the local git repository at `dir` — same
/// trick `syscalls`' own tests use so `RemoteGit::commit_for_remote` can
/// be exercised for real (a real `git clone`/`ls-remote`) without any
/// network access.
fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

/// Opens this repo's own `test_project` fixture (the same one
/// `gui-core`'s tests use) against the real `CoreHandle`'s real
/// background actor — not a fake.
fn open_test_project(harness: &mut Harness<GuiApp>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project");
    open_project_at(harness, &path);
}

/// Same as `open_test_project`, but against a caller-supplied path —
/// for a test that will actually complete a real `Save`, which must run
/// against `scratch_copy_of_test_project`'s writable copy, never the
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

    wait_until(harness, |h| {
        h.query_by_label("No project loaded.").is_none()
    });

    // Every `CollapsingHeader` in the tree (modules and the
    // requirements/tests/results leaf groups alike) now starts collapsed
    // on open — see `render_tree_node`/`render_leaf_group`'s own
    // `default_open(false)`. The tests below exercise leaf-clicking and
    // other tree interactions that assume everything is already visible,
    // not the collapsed-by-default behavior itself (that's its own test,
    // `the_tree_starts_fully_collapsed_when_a_project_first_opens`), so
    // expand everything here once, right after load, via the same
    // "Expand All" button a real user would click.
    harness
        .get_by_role_and_label(Role::Button, "Expand All")
        .click();
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
    harness
        .get_by_role_and_label(Role::Button, leaf_label)
        .click();
    harness.step();
    wait_until(harness, |h| {
        h.query_by_role_and_label(Role::Button, "Edit").is_some()
    });
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(harness, |h| {
        h.query_by_role_and_label(Role::Label, heading).is_some()
    });
}

/// A writable copy of `test_project`, so a test that actually completes
/// a `Save` doesn't write back into the repository's own fixture — same
/// convention (and same reason) as `gui-core`'s own
/// `scratch_copy_of_test_project` in `crates/gui-core/src/actor.rs`'s
/// test module. Caller is responsible for `remove_dir_all`.
fn scratch_copy_of_test_project(label: &str) -> PathBuf {
    let test_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project");
    let dest = std::env::temp_dir().join(format!(
        "gui-ui-interaction-test-{label}-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dest).ok();
    let status = std::process::Command::new("cp")
        .args(["-r", test_project.to_str().unwrap(), dest.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to copy test_project to {dest:?}");
    dest
}

/// A fixed-commit `Git`/`RemoteGit` fake — same shape as (and same reason
/// for) `gui-core`'s own private `FixedGit` in `actor.rs`'s test module,
/// duplicated here since that one isn't `pub` outside its crate. A
/// `scratch_copy_of_test_project` copy lives outside any git repository
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

    fn changed_paths(&self, _dir: &Path) -> Result<Vec<PathBuf>, syscalls::ChangedPathsError> {
        Ok(vec![
            PathBuf::from("root.txt"),
            PathBuf::from("sub/file.txt"),
        ])
    }

    fn commit_all(&self, _dir: &Path, _message: &str) -> Result<(), syscalls::CommitAllError> {
        Ok(())
    }

    fn diff(&self, _dir: &Path, path: &Path) -> Result<String, syscalls::DiffError> {
        let p = path.display();
        Ok(format!(
            "diff --git a/{p} b/{p}\n--- a/{p}\n+++ b/{p}\n@@ -1,1 +1,2 @@\n-old line\n+new line\n+another line\n"
        ))
    }

    fn push(&self, _dir: &Path) -> Result<String, syscalls::PushError> {
        Ok("pushed to origin/main".to_string())
    }
}

impl syscalls::RemoteGit for FixedGit {
    fn commit_for_remote(
        &self,
        _url: &str,
        _path: Option<&Path>,
    ) -> Result<String, syscalls::CommitForRemoteError> {
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
fn opening_a_directory_that_is_not_a_git_repository_shows_an_error_dialog() {
    let mut harness = harness();
    harness.step();

    // A plain directory, never `git init`'d — the real `SystemGit`
    // `CoreHandle::start()` uses reports this as not a repository, which
    // `disk::load_project` now checks before even looking for
    // `project.ron` (see `disk::project::operations::load`'s own
    // `NotAGitRepository` check) — no need for a real project layout to
    // exercise this.
    let dir = std::env::temp_dir().join(format!(
        "gui-ui-interaction-test-not-a-repo-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    harness.state_mut().open_project(dir.clone());
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Couldn't open project:")
            .is_some()
    });

    // The failed load never adopted a project — still the empty state
    // underneath the dialog.
    assert!(harness.query_by_label("No project loaded.").is_some());

    harness.step();
    harness.get_by_role_and_label(Role::Button, "Ok").click();
    harness.step();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Couldn't open project:")
            .is_none()
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn new_project_menu_creates_a_blank_project_and_marks_it_dirty() {
    let mut harness = harness();
    harness.step();
    assert!(harness.query_by_label("No project loaded.").is_some());

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step(); // let the menu popup settle before clicking into it.

    harness
        .get_by_role_and_label(Role::Button, "New Project…")
        .click();
    harness.step();
    harness.step(); // let the modal settle — see this file's module doc.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Project")
            .is_some()
    );

    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness
        .get_all_by_role(Role::TextInput)
        .last()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("Scratch Project");
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Create")
        .click();
    harness.step();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_label("No project loaded.").is_none()
    });
    // The project's own name, from what was just typed — proves a real
    // `Command::NewProject` round-tripped through the actor and came back
    // as a real `Event::TreeChanged`, not just that the empty-state
    // message went away.
    assert!(harness.query_by_label("Scratch Project").is_some());
    // A brand new project has no on-disk home yet — unlike a freshly
    // loaded one, closing without saving would destroy it outright, so
    // it starts dirty (see `apply_outcome`'s `Outcome::NewProject` arm).
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
    // Empty — no requirements/tests/results folders at all (same check
    // as `an_empty_module_shows_no_leaf_group_folders`, but for the
    // project root this time).
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "requirements")
            .is_none()
    );
}

#[test]
fn new_project_dialog_cancel_discards_the_typed_name() {
    let mut harness = harness();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "New Project…")
        .click();
    harness.step();
    harness.step();

    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness
        .get_all_by_role(Role::TextInput)
        .last()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("Abandoned");
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Project")
            .is_none()
    );
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
    let core = gui_core::CoreHandle::start_with(syscalls::StdFilesystem, FixedGit).expect("test tokio runtime");
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
    harness
        .get_by_role_and_label(Role::Button, "New Project…")
        .click();
    harness.step();
    harness.step();
    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness
        .get_all_by_role(Role::TextInput)
        .last()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("Scratch Project");
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Create")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label("Scratch Project").is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
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
    // Requirement text is a required field — a real `AddRequirement`
    // refuses an empty one — and, being a multiline `TextEdit`, reports
    // as `Role::MultilineTextInput` rather than `Role::TextInput` (see
    // the `commit_all` dialog's own comment on this a few lines up).
    // It's the first of the form's three multiline fields (requirement
    // text, requirement guidance, test guidance).
    let multiline_fields: Vec<_> = harness.get_all_by_role(Role::MultilineTextInput).collect();
    multiline_fields[0].focus();
    multiline_fields[0].type_text("Scratch requirement text.");
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Create")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label("\u{e18a} unsaved changes").is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    let dir = std::env::temp_dir().join(format!(
        "gui-ui-interaction-test-new-project-save-as-{}",
        std::process::id()
    ));
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
    let dir = std::env::temp_dir().join(format!(
        "gui-ui-interaction-test-recent-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    let recent_path = dir.join("recent.ron");

    let test_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project");
    let test_project_for_app = test_project.clone();
    let recent_path_for_app = recent_path.clone();
    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start().expect("test tokio runtime"),
            GuiConfig::default(),
            PathBuf::from("/dev/null"),
            RecentProjects::default(),
            recent_path_for_app.clone(),
        )
    });
    harness.step();
    open_project_at(&mut harness, &test_project_for_app);

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
    assert_eq!(recent.paths, vec![test_project.clone()]);

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    // egui appends a "▶"-style submenu arrow glyph to a nested
    // `menu_button`'s own label automatically — found empirically (a
    // plain "Open Recent" query came back empty; dumping every button's
    // label showed "Open Recent ⏵").
    harness
        .get_by_role_and_label(Role::Button, "Open Recent ⏵")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label(test_project.display().to_string().as_str())
            .is_some()
    );

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn clicking_a_recent_project_loads_it() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Open Recent ⏵")
        .click();
    harness.step();
    harness.step();

    let test_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project");
    harness
        .get_by_role_and_label(Role::Button, test_project.display().to_string().as_str())
        .click();
    harness.step();

    // The project was already loaded — re-clicking its own recent entry
    // re-loads the same path, a real (if redundant) `LoadProject` round
    // trip, not a no-op; "Test Project" reappearing after the tree
    // momentarily clears confirms a real reload happened, not that the
    // click did nothing.
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());
    assert!(harness.query_by_label("Test Project").is_some());
}

#[test]
fn unsaved_changes_prompts_before_new_project_and_cancel_leaves_everything_alone() {
    let mut harness = dirty_harness();

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "New Project…")
        .click();
    harness.step();
    harness.step();

    // The confirmation prompt, not the name-entry dialog straight away.
    assert!(
        harness
            .query_by_label("You have unsaved changes. Continue and lose them?")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Project")
            .is_none()
    );

    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("You have unsaved changes. Continue and lose them?")
            .is_none()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Project")
            .is_none()
    );
    // Still the original, still-dirty project — Cancel didn't discard
    // anything.
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
    assert!(harness.query_by_label("Test Project").is_some());
}

#[test]
fn unsaved_changes_continue_opens_the_new_project_dialog() {
    let mut harness = dirty_harness();

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "New Project…")
        .click();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Continue")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("You have unsaved changes. Continue and lose them?")
            .is_none()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Project")
            .is_some()
    );
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
        "New Test Procedure",
        "New Result",
        "New Module",
        "Attachments…",
    ] {
        assert!(
            harness
                .query_by_role_and_label(Role::Button, label)
                .is_some(),
            "missing toolbar button {label:?}"
        );
    }
    assert!(harness.query_by_label("No project loaded.").is_some());
    assert!(
        harness
            .query_by_label(
                "Select an entry in the tree to view it, or use the toolbar to create a new one."
            )
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
            harness
                .query_all_by_role_and_label(Role::Button, label)
                .next()
                .is_some(),
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
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(1600.0, 600.0))
        .build_eframe(|_cc| {
            GuiApp::new(
                gui_core::CoreHandle::start().expect("test tokio runtime"),
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
    let field = harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("zoom field not found");
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
    let field = harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("zoom field not found");
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
    let field = harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("zoom field not found");
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
    let dir = std::env::temp_dir().join(format!(
        "gui-ui-interaction-test-zoom-persist-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("gui-config.ron");
    let config_path_for_app = config_path.clone();

    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start().expect("test tokio runtime"),
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
    let dir = std::env::temp_dir().join(format!(
        "gui-ui-interaction-test-theme-persist-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("gui-config.ron");
    let config_path_for_app = config_path.clone();

    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start().expect("test tokio runtime"),
            GuiConfig::default(),
            config_path_for_app.clone(),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    });
    harness.step();

    harness
        .get_all_by_value("System")
        .next()
        .expect("theme selector not found")
        .click();
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
fn the_spellcheck_checkbox_defaults_to_checked() {
    let mut harness = harness();
    harness.step();

    assert_eq!(
        harness
            .get_by_role_and_label(Role::CheckBox, "Spellcheck")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );
}

#[test]
fn unchecking_spellcheck_persists_to_the_config_file() {
    let dir = std::env::temp_dir().join(format!(
        "gui-ui-interaction-test-spellcheck-persist-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("gui-config.ron");
    let config_path_for_app = config_path.clone();

    let mut harness = Harness::new_eframe(move |_cc| {
        GuiApp::new(
            gui_core::CoreHandle::start().expect("test tokio runtime"),
            GuiConfig::default(),
            config_path_for_app.clone(),
            RecentProjects::default(),
            PathBuf::from("/dev/null"),
        )
    });
    harness.step();

    harness
        .get_by_role_and_label(Role::CheckBox, "Spellcheck")
        .click();
    harness.step();

    assert_eq!(
        harness
            .get_by_role_and_label(Role::CheckBox, "Spellcheck")
            .accesskit_node()
            .toggled(),
        Some(Toggled::False)
    );

    let (loaded, error) = GuiConfig::load(&config_path);
    assert!(error.is_none());
    assert!(!loaded.spellcheck_enabled);

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn typing_and_creating_a_requirement_still_works_once_spellcheck_is_ready() {
    let mut harness = harness();
    open_test_project(&mut harness);

    // Opening the "New Requirement" form is what actually kicks off the
    // background dictionary build (`GuiApp::ensure_spell_checker_started`,
    // called from `render_requirement_form` — deliberately *not*
    // unconditional every frame, see that method's own doc comment on
    // why). Wait for it here, with real wall-clock time between steps,
    // so this test actually exercises the `Ready` rendering path (the
    // custom layouter and right-click popup wiring in
    // `spellchecked_singleline`/`resizable_multiline`) rather than the
    // "still building" passthrough every other interaction test in this
    // file incidentally exercises by moving on before it ever finishes.
    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();
    for _ in 0..100 {
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }

    create_scratch_requirement(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} scratchreq", "Edit Requirement");

    // Confirms the rewritten `.show()`-based rendering (needed to get at
    // `TextEditOutput` for the right-click popup) still displays exactly
    // what was typed, for both the singleline title and the resizable
    // multiline text field.
    assert!(
        harness
            .query(By::new().role(Role::TextInput).value("Scratch Requirement"))
            .is_some()
    );
    assert!(
        harness
            .query(
                By::new()
                    .role(Role::MultilineTextInput)
                    .value("Scratch requirement text.")
            )
            .is_some()
    );
}

#[test]
fn save_is_disabled_with_no_project_loaded_and_enables_once_one_is() {
    let mut harness = harness();
    harness.step();

    let save_button = harness.get_by_role_and_label(Role::Button, "Save");
    assert!(
        save_button.accesskit_node().is_disabled(),
        "Save should start disabled with nothing loaded"
    );

    // "Save As…" only exists in the File menu (no toolbar button for
    // it), so checking it needs the menu open — see this file's module
    // doc on the two-step "let the popup settle" pattern.
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    let save_as_button = harness.get_by_role_and_label(Role::Button, "Save As…");
    assert!(
        save_as_button.accesskit_node().is_disabled(),
        "Save As… should start disabled with nothing loaded"
    );

    // Close the menu again — left open, its own "Save" item would make
    // the toolbar's "Save" query below ambiguous (both share the exact
    // role+label).
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();

    open_test_project(&mut harness);

    let save_button = harness.get_by_role_and_label(Role::Button, "Save");
    assert!(
        !save_button.accesskit_node().is_disabled(),
        "Save should enable once a project is loaded"
    );

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.step();
    let save_as_button = harness.get_by_role_and_label(Role::Button, "Save As…");
    assert!(
        !save_as_button.accesskit_node().is_disabled(),
        "Save As… should enable once a project is loaded"
    );
}

#[test]
fn clicking_save_with_a_known_path_sends_a_real_save_command() {
    // `open_test_project` gives this a real, already-known path (the
    // real `LoadProject` it sends really does complete and populate
    // `project_path` — see `apply_project_path_result`), so
    // `save_button_clicked` takes its non-picker branch here and this
    // click never touches `rfd` — safe to actually click in a headless
    // test, unlike Save with nothing loaded (which would fall back to a
    // real native picker if it weren't disabled) or Save As (which
    // always does).
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

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
    assert!(harness.query_by_label("Test Project").is_some());
}

#[test]
fn new_module_button_opens_a_blank_creation_form() {
    let mut harness = harness();
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "New Module")
        .click();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Module")
            .is_some()
    );
    assert!(harness.query_by_label("Identifier:").is_some());
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Create")
            .is_some()
    );
}

#[test]
fn cancel_closes_the_module_form_and_restores_the_empty_state() {
    let mut harness = harness();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "New Module")
        .click();
    harness.step();
    assert!(harness.query_by_label("Identifier:").is_some());

    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    // Two steps: one to process the click (Cancel lives inside the same
    // render function that already drew "Identifier:" earlier this frame
    // — see this file's module doc comment), one more to see the effect.
    harness.step();
    harness.step();

    assert!(harness.query_by_label("Identifier:").is_none());
    assert!(
        harness
            .query_by_label(
                "Select an entry in the tree to view it, or use the toolbar to create a new one."
            )
            .is_some()
    );
}

#[test]
fn new_requirement_button_opens_the_requirement_form_specifically() {
    let mut harness = harness();
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Requirement")
            .is_some()
    );
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

    harness
        .get_by_role_and_label(Role::Button, "Attachments…")
        .click();
    harness.step();

    // The dialog's own heading, "Attachments" (no ellipsis) — distinct
    // from the toolbar button's "Attachments…" label.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Attachments")
            .is_some()
    );
}

#[test]
fn close_button_closes_the_attachments_dialog() {
    let mut harness = harness();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Attachments…")
        .click();
    harness.step();
    // A newly-opened `egui::Modal` needs one extra frame to "settle"
    // before its own content is reliably clickable — confirmed
    // empirically: without this, the click below on "Close" (which *is*
    // found by the query, and *is* inside the modal) has no effect at
    // all, every time. One `step()` is enough to query the modal's
    // content (as the assert right below shows), but not yet enough to
    // interact with it.
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Attachments")
            .is_some()
    );

    harness.get_by_role_and_label(Role::Button, "Close").click();
    // Two more steps: one to process the click (Close lives inside the
    // same render function that already drew the heading earlier this
    // frame — see this file's module doc comment on the two-step
    // pattern), one more to see the effect.
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Attachments")
            .is_none()
    );
}

/// A harness wired to `FixedGit` (whose `changed_paths` always returns
/// `root.txt`/`sub/file.txt`, see that impl's own comment) against a
/// writable scratch copy of `test_project` — the commit-all dialog needs a
/// real loaded project to enable its toolbar button, but not a real git
/// diff, so `FixedGit` sidesteps depending on this checkout's actual
/// working-tree state.
fn commit_all_harness(dir: &Path) -> Harness<'static, GuiApp> {
    let core = gui_core::CoreHandle::start_with(syscalls::StdFilesystem, FixedGit).expect("test tokio runtime");
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(800.0, 2000.0))
        .build_eframe(|_cc| {
            GuiApp::new(
                core,
                GuiConfig::default(),
                PathBuf::from("/dev/null"),
                RecentProjects::default(),
                PathBuf::from("/dev/null"),
            )
        });
    harness.step();
    open_project_at(&mut harness, dir);
    harness
}

#[test]
fn commit_all_button_opens_the_dialog_with_the_changed_files_list() {
    let dir = scratch_copy_of_test_project("commit-all-opens");
    let mut harness = commit_all_harness(&dir);

    harness
        .get_by_role_and_label(Role::Button, "Commit all changes…")
        .click();
    harness.step();
    harness.step();

    // The dialog's own heading, "Commit all changes" (no ellipsis) —
    // distinct from the toolbar button's "Commit all changes…" label.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Commit all changes")
            .is_some()
    );
    wait_until(&mut harness, |h| h.query_by_label("root.txt").is_some());
    assert!(harness.query_by_label("sub/file.txt").is_some());

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cancel_button_closes_the_commit_all_dialog() {
    let dir = scratch_copy_of_test_project("commit-all-cancel");
    let mut harness = commit_all_harness(&dir);

    harness
        .get_by_role_and_label(Role::Button, "Commit all changes…")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Commit all changes")
            .is_some()
    );

    // `.click_accesskit()`, not `.click()` — the dialog can sit far down
    // the page once the tree above it has real content, pushing Cancel's
    // rect past the harness's simulated viewport, where a real
    // position-based pointer click never lands (see the other
    // `.click_accesskit()` call sites in this file for the same reason).
    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click_accesskit();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Commit all changes")
            .is_none()
    );

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn committing_with_a_message_closes_the_dialog() {
    let dir = scratch_copy_of_test_project("commit-all-commit");
    let mut harness = commit_all_harness(&dir);

    harness
        .get_by_role_and_label(Role::Button, "Commit all changes…")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("root.txt").is_some());

    // A multiline `TextEdit` reports as `Role::MultilineTextInput`, not
    // `Role::TextInput` (see egui's `text_edit/builder.rs`) — the commit
    // message field is the only one of those in the dialog.
    let message_field = harness.get_by_role(Role::MultilineTextInput);
    message_field.focus();
    message_field.type_text("Commit everything");
    harness.step();

    // `.click_accesskit()` — same reason as `cancel_button_closes_the_commit_all_dialog`.
    harness
        .get_by_role_and_label(Role::Button, "Commit")
        .click_accesskit();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Commit all changes")
            .is_none()
    });

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn clicking_a_changed_file_opens_its_diff() {
    let dir = scratch_copy_of_test_project("commit-all-diff-open");
    let mut harness = commit_all_harness(&dir);

    harness
        .get_by_role_and_label(Role::Button, "Commit all changes…")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("root.txt").is_some());

    // `.click_accesskit()` — same "dialog can sit past the simulated
    // viewport" reasoning as the Cancel/Commit buttons above.
    harness.get_by_label("root.txt").click_accesskit();
    harness.step();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Diff: root.txt")
            .is_some()
    });
    // The title above renders synchronously the instant the dialog opens
    // (straight from `dialog.path`, before `Command::GetDiff` is even
    // sent) — it proves the dialog opened, not that the diff arrived. The
    // diff body only appears once the async reply lands and
    // `apply_diff_result` fills it in, so it needs its own `wait_until`,
    // not a bare assert right after the title's.
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("+new line").is_some()
    });
    // `FixedGit::diff`'s own fixed reply (see its doc comment) — a real
    // unified diff round-tripped through `Command::GetDiff`, proving the
    // click sent the request for *this* path and the reply rendered.
    assert!(harness.query_by_label_contains("+new line").is_some());
    assert!(harness.query_by_label_contains("-old line").is_some());

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn closing_the_diff_dialog_leaves_the_commit_all_dialog_open() {
    let dir = scratch_copy_of_test_project("commit-all-diff-close");
    let mut harness = commit_all_harness(&dir);

    harness
        .get_by_role_and_label(Role::Button, "Commit all changes…")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("root.txt").is_some());

    harness.get_by_label("root.txt").click_accesskit();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Diff: root.txt")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Close")
        .click_accesskit();
    harness.step();
    // `wait_until`, not a hardcoded step count — same reasoning as
    // `exit_dialog_saving_then_timeout_lets_the_user_exit_anyway_or_keep_waiting`'s
    // "Exit anyway" close: under CPU contention this has been observed
    // taking longer than a fixed two `step()`s to settle.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Diff: root.txt")
            .is_none()
    });
    // The diff modal closed back to the still-open commit-all dialog, not
    // out entirely — see `GuiApp::diff_dialog_closed`'s own doc comment.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Commit all changes")
            .is_some()
    );

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn push_button_opens_the_confirm_dialog() {
    let dir = scratch_copy_of_test_project("push-opens");
    let mut harness = commit_all_harness(&dir);

    harness.get_by_role_and_label(Role::Button, "Push…").click();
    harness.step();
    harness.step();

    // The dialog's own heading, "Push" — distinct from the toolbar
    // button's "Push…" label (see `render_push_dialog`'s own doc
    // comment on why the two differ).
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Push")
            .is_some()
    );
    assert!(
        harness
            .query_by_label("Push the current branch to its remote?")
            .is_some()
    );
    // Confirm state only — nothing sent to `gui-core` yet, so the commit-
    // all dialog (which the same toolbar row also opens) was untouched.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Commit all changes")
            .is_none()
    );

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn push_dialog_warns_when_there_is_nothing_to_push() {
    let dir = scratch_copy_of_test_project("push-nothing-to-push");
    let mut harness = commit_all_harness(&dir);

    harness.get_by_role_and_label(Role::Button, "Push…").click();
    harness.step();
    harness.step();

    // `FixedGit::unpushed_commits`'s default (empty) reply — a real round
    // trip through `Command::GetUnpushedCommits`, proving the preview
    // fetch fired alongside the dialog opening and rendered its warning.
    wait_until(&mut harness, |h| {
        h.query_by_label("No commits to push.").is_some()
    });

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cancel_button_closes_the_push_dialog_without_pushing() {
    let dir = scratch_copy_of_test_project("push-cancel");
    let mut harness = commit_all_harness(&dir);

    harness.get_by_role_and_label(Role::Button, "Push…").click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Push")
            .is_some()
    );

    // `.click_accesskit()` — same "dialog can sit past the simulated
    // viewport" reasoning as the commit-all dialog's own Cancel button.
    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click_accesskit();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Push")
            .is_none()
    );

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn clicking_push_shows_the_output() {
    let dir = scratch_copy_of_test_project("push-click");
    let mut harness = commit_all_harness(&dir);

    harness.get_by_role_and_label(Role::Button, "Push…").click();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Push")
        .click_accesskit();
    harness.step();

    // `FixedGit::push`'s own fixed reply (see its doc comment) — a real
    // round trip through `Command::Push`, proving the click sent the
    // request and the reply rendered.
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pushed to origin/main").is_some()
    });
    // The confirm/push button is gone once output is showing — only
    // "Close" remains (see `render_push_dialog`'s own doc comment).
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Push")
            .is_none()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Close")
            .is_some()
    );

    drop(harness);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn opening_a_real_project_populates_the_tree() {
    let mut harness = harness();
    harness.step();
    assert!(harness.query_by_label("No project loaded.").is_some());

    open_test_project(&mut harness);

    assert!(harness.query_by_label("No project loaded.").is_none());
    // The project's own name, from `test_project/project.ron` — proves
    // real data came back from a real `LoadProject`, not just that the
    // placeholder message went away.
    assert!(harness.query_by_label("Test Project").is_some());
}

/// Creates a real requirement named "scratchreq" at the project root with
/// non-empty text, via the actual New Requirement form (same field-index
/// convention `a_successful_requirement_create...`'s own new-project test
/// uses) — the Copy/Paste tests below need a source requirement with real
/// text, since a real `AddRequirement` refuses an empty one (see
/// `logical::draft::module::add_requirement`) and `test_project`'s own
/// fixture requirements all happen to have empty `requirement.typ` files
/// (this project's fixture is deliberately structural, not textual).
/// Leaves the tree filtered to nothing and the "New Requirement" form
/// closed (back to the read-only viewer) once done.
fn create_scratch_requirement(harness: &mut Harness<GuiApp>) {
    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();
    // Field order among `Role::TextInput` nodes in create mode: the status
    // bar's own zoom field(0) and the left pane's own filter field(1) —
    // both always first, rendered before the center pane — then name(2),
    // title(3). Same convention `a_successful_requirement_create...`'s own
    // new-project test relies on.
    let fields: Vec<_> = harness.get_all_by_role(Role::TextInput).collect();
    fields[2].focus();
    fields[2].type_text("scratchreq");
    fields[3].focus();
    fields[3].type_text("Scratch Requirement");
    let multiline_fields: Vec<_> = harness.get_all_by_role(Role::MultilineTextInput).collect();
    multiline_fields[0].focus();
    multiline_fields[0].type_text("Scratch requirement text.");
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Create")
        .click();
    harness.step();
    wait_until(harness, |h| {
        h.query_by_label("\u{e18a} unsaved changes").is_some()
    });
}

#[test]
fn right_click_copy_then_paste_duplicates_a_requirement_into_another_module() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    // Right-click the freshly created "scratchreq" requirement leaf and
    // Copy it. `"\u{e32c} scratchreq"`, not bare `"scratchreq"` — a
    // requirement leaf's own accessible label prepends its status icon
    // (see `render_leaf`'s `Atoms` construction) — matches this file's
    // existing convention for querying requirement leaves elsewhere.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Copy")
        .click_accesskit();
    harness.step();

    // `test_project`'s "beta" module has no requirements and no
    // submodules of its own (see its on-disk fixture) — the tree's
    // right-click "Paste" targets its plain-`Label` row (the no-
    // submodules branch of `render_tree_node`), not a `CollapsingHeader`.
    harness.get_by_label("beta").click_secondary();
    harness.step();
    harness.step();
    // "Copy" fetches the requirement's full content via a real, genuinely
    // async `GetEntryDetail` round trip (see `copy_requirement_clicked`)
    // rather than completing inline — `Paste` starts out disabled
    // (`attach_paste_requirement_menu`) and only becomes clickable once
    // that reply actually lands, so wait for it instead of assuming a
    // fixed `step()` count was enough.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Paste")
            .is_some_and(|node| !node.accesskit_node().is_disabled())
    });
    harness
        .get_by_role_and_label(Role::Button, "Paste")
        .click_accesskit();
    harness.step();

    // Switch to "beta" as the current module (its own "not current" glyph
    // button — same lookup `module_page_shows_summary_then_renames_a_real_module`
    // uses) and wait for the real, async `AddRequirement` reply to land:
    // an empty module shows no "requirements" leaf-group header at all
    // (see `render_leaf_group`), so its mere appearance proves the paste
    // actually landed in *this* module, not just somewhere.
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .last()
        .expect("no module buttons found")
        .click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "requirements (1)")
            .is_some()
    });
    // Beta's own "requirements" group is brand new, but "requirements"
    // groups start expanded by default (`render_leaf_group`'s
    // `default_open(kind == EntryKind::Requirement)`), so its leaf is
    // already rendered/queryable without an extra click to open it.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_some()
    );
}

#[test]
fn right_click_recreate_renames_a_requirement_and_regenerates_its_title() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    // Right-click the freshly created "scratchreq" requirement leaf and
    // choose "Recreate…" — same lookup convention as the Copy/Paste test
    // above.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Recreate…")
        .click_accesskit();
    harness.step();

    // Unlike the form's own "Recreate…" button, the tree's context-menu
    // version has no open form to read from — it fetches the requirement's
    // full content via a real `GetEntryDetail` round trip
    // (`recreate_requirement_from_tree_clicked`) rather than completing
    // inline, so wait for the modal instead of assuming a fixed `step()`
    // count was enough.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Recreate Requirement")
            .is_some()
    });

    // "Regenerate title from new name" is checked by default — leave it
    // alone and just change the name, then confirm the regenerated title
    // shows up once the recreate actually lands.
    let name_field = harness
        .get_all(By::new().role(Role::TextInput).value("scratchreq"))
        .next()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("_renamed");
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Recreate")
        .click();
    harness.step();

    // The recreate flow chains a real `FindReferences`, then
    // `RemoveRequirement`, then `AddRequirement` over the background
    // actor — wait for the tree to actually show the new name rather than
    // assuming a fixed `step()` count covers every leg.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "\u{e32c} scratchreq_renamed")
            .is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_none(),
        "the old name should be gone, not just a second copy added"
    );

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq_renamed")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label("Scratchreq Renamed").is_some()
    });
}

#[test]
fn the_edit_form_title_field_has_a_button_that_regenerates_it_from_the_identifier() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} scratchreq", "Edit Requirement");

    // Title starts out as whatever `create_scratch_requirement` typed
    // ("Scratch Requirement"), independent of the identifier
    // ("scratchreq") — clicking the regenerate button next to the Title
    // field should overwrite it with `title_case_from_name("scratchreq")`
    // regardless, same glyph (`\u{e094}`, `icons::REGENERATE_TITLE`) as
    // `icons::UPDATE_STALE_REFERENCES` uses elsewhere. The title field is
    // a live `TextEdit`, not a `Label`, so look it up by its current
    // value like the recreate-dialog's own name field does above.
    assert!(
        harness
            .get_all(By::new().role(Role::TextInput).value("Scratch Requirement"))
            .next()
            .is_some()
    );
    harness
        .get_by_role_and_label(Role::Button, "\u{e094}")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .get_all(By::new().role(Role::TextInput).value("Scratchreq"))
            .next()
            .is_some()
    );
    assert!(
        harness
            .query_all(By::new().role(Role::TextInput).value("Scratch Requirement"))
            .next()
            .is_none()
    );
}

#[test]
fn pressing_enter_in_the_recreate_name_field_submits_like_clicking_recreate() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Recreate…")
        .click_accesskit();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Recreate Requirement")
            .is_some()
    });

    let name_field = harness
        .get_all(By::new().role(Role::TextInput).value("scratchreq"))
        .next()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("_renamed");
    harness.step();

    // Enter in the name field, not a click on "Recreate", drives the rest
    // of this test — same real `FindReferences`/`RemoveRequirement`/
    // `AddRequirement` chain as the click-driven version above, just
    // triggered by `render_recreate_requirement_dialog`'s own
    // `enter_pressed_in_name_field` handling instead.
    harness.key_press(egui::Key::Enter);
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "\u{e32c} scratchreq_renamed")
            .is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_none(),
        "the old name should be gone, not just a second copy added"
    );
}

#[test]
fn pressing_enter_in_the_recreate_name_field_does_nothing_when_recreate_would_be_disabled() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Recreate…")
        .click_accesskit();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Recreate Requirement")
            .is_some()
    });

    // The name field starts out prefilled with the unchanged current name
    // ("scratchreq") — Recreate is disabled until it actually differs, so
    // Enter here must not submit either.
    let name_field = harness
        .get_all(By::new().role(Role::TextInput).value("scratchreq"))
        .next()
        .expect("name field not found");
    name_field.focus();
    harness.step();

    harness.key_press(egui::Key::Enter);
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Recreate Requirement")
            .is_some(),
        "the dialog should still be open — Enter must not bypass the unchanged-name guard"
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_some()
    );
}

#[test]
fn right_click_duplicate_prompts_for_a_name_then_creates_the_copy() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Duplicate")
        .click_accesskit();
    harness.step();

    // "Duplicate" fetches the requirement's full content via a real
    // `GetEntryDetail` round trip (`duplicate_requirement_clicked`) before
    // it can even open the prompt (`open_duplicate_requirement_dialog`) —
    // wait for the modal itself rather than assuming a fixed `step()`
    // count was enough.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Duplicate Requirement")
            .is_some()
    });
    // Pre-filled with `unique_copy_name`'s own suggestion — left as-is,
    // proving the field really is editable/confirmable rather than
    // asserting a hardcoded value here.
    assert!(
        harness
            .query_all(By::new().role(Role::TextInput).value("scratchreq copy"))
            .next()
            .is_some()
    );
    // "Regenerate title from new name" is checked by default, same as the
    // Recreate dialogs' own checkbox.
    assert_eq!(
        harness
            .get_by_role_and_label(Role::CheckBox, "Regenerate title from new name")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );
    // Same "a freshly opened `egui::Modal` needs one extra `step()` before
    // its own content is reliably clickable" gotcha as the Delete
    // confirmation test above.
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Duplicate")
        .click();
    harness.step();

    // The confirm click sends a real `AddRequirement` for the deduped
    // copy — wait for the tree to pick up the new leaf.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "\u{e32c} scratchreq copy")
            .is_some()
    });
    // The original is untouched — Duplicate adds a new leaf alongside it,
    // unlike Recreate which replaces the old one.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_some()
    );

    // The default-checked "Regenerate title from new name" should have
    // overwritten the copy's title (originally "Scratch Requirement",
    // carried over unchanged from the source) with one derived from the
    // new name instead — `title_case_from_name` title-cases every
    // whitespace-separated word, not just the leading one.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq copy")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label("Scratchreq Copy").is_some()
    });
}

#[test]
fn duplicate_dialog_rejects_a_name_that_already_exists() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Duplicate")
        .click_accesskit();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Duplicate Requirement")
            .is_some()
    });
    harness.step();

    let name_field = harness
        .get_all(By::new().role(Role::TextInput).value("scratchreq copy"))
        .next()
        .expect("name field not found");
    name_field.focus();
    // Ctrl+A then typing over the selection replaces the pre-filled
    // suggestion outright rather than appending to it, so the field ends
    // up exactly "scratchreq" — the sibling that already exists.
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    name_field.type_text("scratchreq");
    harness.step();

    let duplicate_button = harness.get_by_role_and_label(Role::Button, "Duplicate");
    assert!(
        duplicate_button.accesskit_node().is_disabled(),
        "Duplicate should be disabled while the name collides with an existing sibling"
    );
    assert!(
        harness
            .query_by_label("\"scratchreq\" already exists in this module.")
            .is_some()
    );
}

#[test]
fn right_click_delete_removes_a_requirement_after_confirmation() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Delete…")
        .click_accesskit();
    harness.step();

    // Same "no open form needed" reasoning as Recreate/Duplicate above —
    // unlike the edit form's own Delete button, this reaches the
    // confirmation straight from the tree, so wait for the modal itself
    // rather than assuming a fixed `step()` count.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Delete?").is_some()
    });
    // Same "a freshly opened `egui::Modal` needs one extra `step()` before
    // its own content is reliably clickable" gotcha noted at the top of
    // this file — the modal opened on the very same step "Delete…" was
    // clicked (no async round trip involved, unlike Recreate/Duplicate
    // above), so it hasn't had the chance to "settle" the way `wait_until`
    // normally provides for free by polling across several steps.
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Delete")
        .click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_none()
    });
}

#[test]
fn pasting_the_same_requirement_twice_dedupes_the_seconds_name() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    // Copy root's "scratchreq" and paste it into "beta" twice in a row —
    // deliberately never switching the current module away from root in
    // between, so root's own "scratchreq" button (the thing being
    // re-copied each time) stays reachable throughout: the top tree pane
    // never shows leaves at all, and the bottom pane only shows whichever
    // module is *current*, which pasting itself never changes.
    for _ in 0..2 {
        harness
            .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .click_secondary();
        harness.step();
        harness.step();
        harness
            .get_by_role_and_label(Role::Button, "Copy")
            .click_accesskit();
        harness.step();

        harness.get_by_label("beta").click_secondary();
        harness.step();
        harness.step();
        // See the sibling test's comment on why `Paste` needs a real
        // wait, not a fixed `step()` count — `Copy`'s `GetEntryDetail`
        // fetch is genuinely async.
        wait_until(&mut harness, |h| {
            h.query_by_role_and_label(Role::Button, "Paste")
                .is_some_and(|node| !node.accesskit_node().is_disabled())
        });
        harness
            .get_by_role_and_label(Role::Button, "Paste")
            .click_accesskit();
        harness.step();
        harness.step();
    }

    // Switch to "beta" and confirm it ended up with both: the first
    // paste's "scratchreq" and a second one `unique_copy_name` deduped to
    // "scratchreq copy" once "scratchreq" was already taken there.
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .last()
        .expect("no module buttons found")
        .click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "requirements (2)")
            .is_some()
    });
    // "requirements" groups start expanded by default
    // (`render_leaf_group`'s `default_open(kind ==
    // EntryKind::Requirement)`), so this brand-new group's leaves are
    // already reachable without an extra click to open it.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} scratchreq copy")
            .is_some()
    );
}

#[test]
fn pasting_via_the_requirements_leaf_group_header_targets_the_current_module() {
    let mut harness = harness();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
        .click_secondary();
    harness.step();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Copy")
        .click_accesskit();
    harness.step();

    // Switch to "beta" as the current module *first*, then paste via the
    // bottom pane's own "requirements" leaf-group header instead of going
    // back to the top tree pane's per-module row — the point of this
    // second paste entry point (`render_leaf_group`'s own
    // `attach_paste_requirement_menu` call).
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .last()
        .expect("no module buttons found")
        .click();
    harness.step();

    // "beta" has zero requirements, so ordinarily its "requirements"
    // group wouldn't render at all (`render_leaf_group` returns early) —
    // it shows up here as "requirements (0)" specifically because
    // something is on the clipboard to paste (see that function's own
    // comment on `can_paste_here`). `wait_until` rather than assuming the
    // single `step()` above already settled it — under CPU contention a
    // panicking getter immediately after just one `step()` has been
    // observed to run a beat too early.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "requirements (0)")
            .is_some()
    });
    harness
        .get_by_role_and_label(Role::Button, "requirements (0)")
        .click_secondary();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Paste")
            .is_some_and(|node| !node.accesskit_node().is_disabled())
    });
    harness
        .get_by_role_and_label(Role::Button, "Paste")
        .click_accesskit();
    harness.step();

    // "requirements" groups start expanded by default
    // (`render_leaf_group`'s `default_open(kind ==
    // EntryKind::Requirement)`), so once the paste lands there's no need
    // for an extra click to open it before its leaf is queryable.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "\u{e32c} scratchreq")
            .is_some()
    });
}

#[test]
fn the_tree_starts_fully_collapsed_when_a_project_first_opens() {
    let mut harness = harness();
    harness.step();

    // Deliberately not `open_test_project` — that helper clicks
    // "Expand All" right after load for every *other* test's benefit
    // (see its own doc comment). This test exercises the real opened
    // state before any such click, so it drives `open_project` directly.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_project");
    harness.state_mut().open_project(path);
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    // The tree loaded at all — "beta" (a childless module, so a plain
    // `Label` rather than a collapsible `CollapsingHeader`, see
    // `render_tree_node`'s two branches) is a convenient proof of that.
    // The "requirements" leaf group folder starts expanded (see
    // `render_leaf_group`'s own `default_open(kind ==
    // EntryKind::Requirement)`) so its children are reachable
    // immediately, but "test procedures" still starts collapsed.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "beta")
            .is_some()
    );
    assert!(leaf_group_button_present(&harness, "requirements"));
    assert!(leaf_group_button_present(&harness, "test procedures"));
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );

    // "Expand All" reveals it, proving the leaf was only hidden by the
    // collapsed header, not missing from the tree entirely.
    harness
        .get_by_role_and_label(Role::Button, "Expand All")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );
}

#[test]
fn the_tree_groups_leaves_under_requirements_and_test_procedures_folders() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    // The two folders themselves — root-level, since `test_project` has
    // root-level requirements/tests, not just ones nested in a submodule.
    // A `CollapsingHeader`'s own label reports as `Role::Button` (it's
    // clickable, toggling expand/collapse), same as a module's own name —
    // not `Role::Label`. There's no separate "results" folder any more —
    // a requirement's results are nested under it (see
    // `disk::RequirementOnDisk::results`), not a flat sibling group.
    assert!(leaf_group_button_present(&harness, "requirements"));
    assert!(leaf_group_button_present(&harness, "test procedures"));
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "results")
            .is_none()
    );

    // `open_project_at` already clicked "Expand All" — a real leaf
    // underneath is visible and clickable. "design" is a real
    // root-level requirement; its tree label carries the
    // unvalidated-status glyph.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );

    // "design"'s own nested result (also named "design" in the fixture)
    // is deliberately not shown as a tree row — results are only reachable
    // from their owning requirement's detail panel.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "design")
            .is_none()
    );
}

#[test]
fn an_empty_module_shows_no_leaf_group_folders() {
    let mut harness = dirty_harness();

    // `dirty_harness` creates "interaction_test_module" at the root but
    // never selects it — the bottom pane only ever shows the *selected*
    // module's own leaves now (`render_selected_module_pane`), so an
    // unselected module (empty or not) never contributes a leaf group
    // either way; leaving it unselected wouldn't actually prove anything
    // about *this* module being empty. Select it directly instead, which
    // is what really exercises `render_leaf_group`'s "omit an empty group
    // entirely" behavior — and, since selecting it also swaps the bottom
    // pane away from root, root's own three groups (`requirements`/
    // `tests`/`results`) should disappear too.
    //
    // `ModuleDraft::modules` is a `BTreeMap`, so the tree renders modules
    // in sorted order, and "interaction_test_module" sorts after every
    // other root-level module — it's reliably the *last* "set as current
    // module" glyph button in tree order regardless of how many other
    // not-current modules (or their own submodules, like alpha's own
    // "alpha_child") come before it (see
    // `module_page_shows_summary_then_renames_a_real_module`'s own
    // comment on this exact glyph/pattern, `\u{E24A}` / Phosphor's
    // `FOLDER_NOTCH`, shared by every not-current module's row).
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .last()
        .expect("interaction_test_module's module button not found")
        .click();
    harness.step();
    harness.step();

    // `get_all_by_...` panics on zero matches (it's a "must find at
    // least one" query, per `kittest::query::get_all`) — `query_all_by_...`
    // is the right tool here since we're asserting *absence*.
    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "requirements")
            .count(),
        0
    );
    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "test procedures")
            .count(),
        0
    );
    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "results")
            .count(),
        0
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "interaction_test_module")
            .is_some()
    );
}

#[test]
fn typing_into_the_filter_bar_hides_non_matching_leaves_and_modules() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    // Both the "design" requirement and the "external" requirement are
    // real root-level leaves in `test_project` before any filtering.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} external")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "beta")
            .is_some()
    );

    // Index 0 is the zoom field (see `zoom_field_value`'s own comment);
    // the filter field is the next `TextInput` in tree order, drawn
    // right after it in the status bar/left pane.
    let filter_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(1)
        .expect("filter field not found");
    filter_field.focus();
    filter_field.type_text("design");
    harness.step();
    harness.step();

    // The matching leaf survives; a non-matching sibling leaf and every
    // module (none of which has a "design"-named descendant in
    // `test_project`) are filtered out.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} external")
            .is_none()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "beta")
            .is_none()
    );

    // Clearing the filter (via the "×" button) restores full visibility.
    harness.get_by_role_and_label(Role::Button, "×").click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} external")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "beta")
            .is_some()
    );
}

#[test]
fn filtering_by_module_name_hides_non_matching_modules_in_top_tree() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "beta")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "alpha")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );

    let filter_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(1)
        .expect("filter field not found");
    filter_field.focus();
    filter_field.type_text("beta");
    harness.step();
    harness.step();

    // Only the matching module survives in the top tree — `alpha` doesn't
    // contain "beta" anywhere in its own module path, so it's gone.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "beta")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "alpha")
            .is_none()
    );
    // The bottom pane's own leaf groups still go through the shared,
    // unchanged `node_matches_filter` (per the treeview-rework plan) —
    // "design"'s full path doesn't contain "beta" either, so it's
    // filtered out right along with it.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_none()
    );

    harness.get_by_role_and_label(Role::Button, "×").click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "alpha")
            .is_some()
    );
}

#[test]
fn the_top_tree_shows_empty_modules_and_no_leaves() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    // "beta" is a real root-level module in `test_project` with no
    // requirements/tests/results/submodules of its own — it still shows
    // up in the module-only top tree, as a plain, uncollapsible `Label`
    // rather than a `CollapsingHeader` (see `render_tree_node`'s two
    // branches: only a module with its own submodules gets one).
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "beta")
            .is_some()
    );
    assert_eq!(
        harness
            .get_all(By::new().role(Role::Button).label_contains("requirements"))
            .count(),
        1
    );

    // Expanding "alpha"'s own `CollapsingHeader` in the top tree (unlike
    // "beta", it has a real submodule of its own, "alpha_child", so it
    // actually is one) used to reveal a per-module "requirements"/"tests"
    // nested group (the old `render_module_children`'s recursive leaf
    // rendering, at every depth) — `render_module_children` is module-only
    // now, so expanding it adds nothing: the count of leaf-group headers
    // (all living in the bottom pane now, one set for the selected
    // module) stays exactly one no matter which modules get expanded.
    harness.get_by_role_and_label(Role::Button, "alpha").click();
    harness.step();
    harness.step();

    assert_eq!(
        harness
            .get_all(By::new().role(Role::Button).label_contains("requirements"))
            .count(),
        1
    );
    assert_eq!(
        harness
            .get_all(
                By::new()
                    .role(Role::Button)
                    .label_contains("test procedures")
            )
            .count(),
        1
    );
}

#[test]
fn selecting_a_module_shows_its_own_leaves_in_the_bottom_pane() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    // Root is selected by default right after load — its own real
    // root-level leaves are showing in the bottom pane.
    assert!(leaf_group_button_present(&harness, "requirements"));
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );

    // "beta" (sorted 2nd of 2 root modules: alpha, beta — see
    // `module_page_shows_summary_then_renames_a_real_module`'s
    // own comment on this glyph/pattern) has none of its own — selecting
    // it swaps the bottom pane over to *its* (empty) leaves, not root's.
    // `open_test_project` clicks "Expand All", so alpha's own child
    // module `alpha_child` is also expanded and rendered between alpha
    // and beta — the non-current-module glyph buttons are, in order,
    // alpha, alpha_child, beta, so beta is the *third* (`nth(2)`), not
    // second.
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .nth(2)
        .expect("beta's module button not found")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_all_by_label("Module: beta").next().is_some()
    });

    assert!(
        harness
            .query_by_role_and_label(Role::Button, "requirements")
            .is_none()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_none()
    );
}

#[test]
fn switching_selected_module_updates_the_bottom_pane() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );

    // Select "beta" (empty) — this is the regression the bottom pane
    // used to have: it kept showing root's own leaves no matter which
    // module was actually selected. Confirm it really does swap away.
    //
    // `open_test_project`'s "Expand All" also expands alpha's own child
    // module `alpha_child`, which renders between alpha and beta, so
    // beta is the *third* non-current-module glyph button (`nth(2)`).
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .nth(2)
        .expect("beta's module button not found")
        .click();
    // Clicking a tree-pane glyph mutates `selected_module` partway through
    // that frame's render, after the tree itself (including this very row)
    // has already been drawn with the old value — the click only lands in
    // time for widgets rendered later that same frame, like the module
    // page's own heading. A second `step()` lets the tree pane repaint
    // with the now-current `selected_module`, matching the pattern already
    // used by `module_page_shows_summary_then_renames_a_real_module` and
    // `the_top_tree_shows_empty_modules_and_no_leaves`.
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_all_by_label("Module: beta").next().is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_none()
    );

    // Switch back to root via its own row's "set as current module"
    // button (rendered first, right next to the "Test Project" name — see
    // `render_left_pane`) — now the only remaining `\u{E24A}`-glyph
    // button belonging to "beta" is gone (it's current now, so it shows
    // `MODULE_CURRENT` instead), so root's is reliably first.
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .next()
        .expect("root's own module button not found")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    });

    assert!(leaf_group_button_present(&harness, "requirements"));
}

#[test]
fn back_forward_navigation_updates_the_bottom_pane() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    );

    // `open_test_project`'s "Expand All" also expands alpha's own child
    // module `alpha_child`, which renders between alpha and beta, so
    // beta is the *third* non-current-module glyph button (`nth(2)`).
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .nth(2)
        .expect("beta's module button not found")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_all_by_label("Module: beta").next().is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_none()
    );

    // Opening a project lands on its own root page first (history stop
    // #0 — see `back_and_forward_toolbar_buttons_round_trip_two_real_selections`'s
    // own comment), so Back from here returns to root and the bottom pane
    // should come back with it.
    harness.get_by_role_and_label(Role::Button, "Back").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Forward")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_all_by_label("Module: beta").next().is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_none()
    );
}

#[test]
fn the_bottom_pane_lists_attachments_and_templates_read_only() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| h.query_by_label("Test Project").is_some());

    // Root is selected by default; `sidebar_pools`' own `GetModulePools`
    // fetch is a separate round trip from the tree data itself (fired
    // from `Event::TreeChanged`, landing later), so wait for its own
    // header rather than assuming it's already back.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "attachments")
            .is_some()
    });

    // `open_test_project`'s own "Expand All" click already happened
    // before this async data arrived, so the attachments/templates
    // groups' own `CollapsingHeader`s still start collapsed — expand
    // them directly.
    harness
        .get_by_role_and_label(Role::Button, "attachments")
        .click();
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "templates")
        .click();
    harness.step();
    harness.step();

    // Real files from `test_project/attachments/` and
    // `test_project/templates/` — plain read-only labels
    // (`render_pool_group`), not a `Role::Button` like a tree leaf.
    // `render_pool_group` renders each path via `path.display().to_string()`,
    // so the nested attachment shows its full relative path, not just its
    // basename.
    assert!(harness.query_by_label("overview.md").is_some());
    assert!(
        harness
            .query_by_label("diagrams/architecture.txt")
            .is_some()
    );
    assert!(harness.query_by_label("report-template.typ").is_some());
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "overview.md")
            .is_none()
    );
}

/// Opens `test_project`, creates a module through the real toolbar/form
/// flow, and waits for the resulting `Outcome::AddModule(Ok(()))` to mark
/// the project dirty — the shared setup for every test below that needs a
/// real dirty project to start from.
fn dirty_harness<'a>() -> Harness<'a, GuiApp> {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Module")
        .click();
    harness.step();
    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness
        .get_all_by_role(Role::TextInput)
        .last()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("interaction_test_module");
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Create")
        .click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_label("\u{e18a} unsaved changes").is_some()
    });
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
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "interaction_test_module")
            .is_some()
    );
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Undo")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        harness
            .get_by_role_and_label(Role::Button, "Redo")
            .accesskit_node()
            .is_disabled()
    );

    harness.get_by_role_and_label(Role::Button, "Undo").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "interaction_test_module")
            .is_none()
    });

    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Redo")
            .accesskit_node()
            .is_disabled()
    );

    harness.get_by_role_and_label(Role::Button, "Redo").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "interaction_test_module")
            .is_some()
    });
}

#[test]
fn undo_is_disabled_with_no_project_loaded() {
    let mut harness = harness();
    harness.step();

    assert!(
        harness
            .get_by_role_and_label(Role::Button, "Undo")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        harness
            .get_by_role_and_label(Role::Button, "Redo")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn exit_with_unsaved_changes_shows_the_confirmation_dialog() {
    let mut harness = dirty_harness();

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_some()
    );
    // "Discard"/"Cancel" are unique to this dialog; "Save" isn't checked
    // here — the toolbar's own persistent Save button shares that exact
    // role+label, so it's an ambiguous query while both are visible at
    // once (confirmed empirically: `query_by_role_and_label` panics on
    // "found two or more nodes"). The dialog's own message text already
    // confirms it's showing.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Discard")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Cancel")
            .is_some()
    );
}

#[test]
fn cancel_on_the_exit_dialog_dismisses_it_and_stays_open() {
    let mut harness = dirty_harness();
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_some()
    );

    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_none()
    );
    // Cancelling the exit is not the same as discarding the edit — still
    // dirty, still showing the normal toolbar.
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "New Module")
            .is_some()
    );
}

#[test]
fn discard_on_the_exit_dialog_closes_it_and_proceeds() {
    let mut harness = dirty_harness();
    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_some()
    );

    harness
        .get_by_role_and_label(Role::Button, "Discard")
        .click();
    harness.step();
    harness.step();

    // Stage 2 (the actual `Command::Shutdown` + viewport close) is
    // exercised exhaustively at the logic level in `src/lib.rs`'s own
    // tests (`take_ready_to_exit`, `discard_proceeds_to_exit_without_saving`);
    // what this proves at the rendering level is that a real click on the
    // real "Discard" button really does resolve the dialog, which is the
    // part those tests can't see.
    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_none()
    );
}

/// Simulates the OS window-close control (the title bar "X", Alt-F4,
/// Cmd-Q, ...) by injecting a `ViewportEvent::Close` into the root
/// viewport's `ViewportInfo`, the same event `winit` delivers for a real
/// close click — see `egui::ViewportInfo::close_requested`'s own doc
/// comment on what it reads.
fn simulate_close_button_click(harness: &mut Harness<GuiApp>) {
    harness
        .input_mut()
        .viewports
        .get_mut(&egui::ViewportId::ROOT)
        .expect("root viewport info missing")
        .events
        .push(egui::ViewportEvent::Close);
}

#[test]
fn window_close_button_with_unsaved_changes_shows_the_confirmation_dialog() {
    let mut harness = dirty_harness();

    simulate_close_button_click(&mut harness);
    harness.step();
    harness.step();

    // Cancelled, not honored outright — the dialog is up and the app is
    // still running, proving the close was intercepted rather than
    // falling through to eframe's default "just close" behavior.
    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Discard")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Cancel")
            .is_some()
    );
}

#[test]
fn cancelling_the_window_close_dialog_leaves_the_project_open() {
    let mut harness = dirty_harness();

    simulate_close_button_click(&mut harness);
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_some()
    );

    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_none()
    );
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "New Module")
            .is_some()
    );
}

#[test]
fn window_close_button_with_no_unsaved_changes_skips_the_dialog() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    simulate_close_button_click(&mut harness);
    harness.step();

    // Stage 1 is skipped entirely when `dirty` is `false` (see
    // `on_exit_clicked`): no confirmation dialog appears at all — Stage 2
    // (`Command::Shutdown` + re-requesting the viewport close) is covered
    // at the logic level by `src/lib.rs`'s own `take_ready_to_exit` tests.
    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_none()
    );
}

#[test]
fn a_validated_requirements_tree_leaf_shows_the_unmet_status_icon() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Before validating, every requirement is `EntryStatus::Unvalidated`
    // (see the test above) — `Validate` actually resolves real Met/Unmet
    // status via `logical::validate`. None of `test_project`'s
    // requirements have a passing `Result` wired up to satisfy them, so
    // every one of them (including "design") comes back `Unmet` —
    // confirmed empirically, not a fixture property documented elsewhere.
    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    // `\u{e4f8}` is `icons::status_icon`'s `X_CIRCLE` (Unmet) — the same
    // icon+color-chip path `render_leaf` now runs for every requirement
    // via `theme_colors::status_colors`, exercised here via a real
    // `EntryStatus::Unmet` rather than the default `Unvalidated` every
    // other test in this file sees.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e4f8} design")
            .is_some()
    );
    // The old unvalidated-status icon is gone now that it's actually
    // Unmet.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "\u{e32c} design")
            .is_none()
    );
}

#[test]
fn the_requirement_viewer_explains_why_it_is_unmet() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    // Same real, fact-checked outcome as
    // `a_validated_requirements_tree_leaf_shows_the_unmet_status_icon` —
    // "design"'s own pinned test reference
    // (`2b3c4d5e6f708192a0b1c2d3e4f5061728394a5b`, in `requirement.ron`)
    // no longer matches `tests/smoke`'s real current commit
    // in this repo's own git history, so it's Unmet with a genuine stale-
    // reference reason, not a placeholder.
    harness
        .get_by_role_and_label(Role::Button, "\u{e4f8} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    assert!(harness.query_by_label("Status:").is_some());
    assert!(harness.query_by_label("Unmet").is_some());
    assert!(
        harness
            .query_by_label_contains("Test procedure \"tests/smoke\": its reference is stale")
            .is_some()
    );
}

#[test]
fn clicking_validate_refreshes_an_already_open_requirement_viewer() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Open "design"'s viewer *first*, before validating — it starts
    // `Unvalidated` (the project hasn't been validated in this session
    // yet), same starting point `selecting_an_existing_requirement_opens_its_read_only_viewer`
    // documents.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });
    assert!(harness.query_by_label("Unvalidated").is_some());

    // Validate *without navigating away* — the still-open viewer above is
    // the thing under test, not a freshly reopened one (that path is
    // already covered by `the_requirement_viewer_explains_why_it_is_unmet`).
    // This is the regression test for the gap `GuiApp::apply_outcome`
    // used to have no `Outcome::Validate` arm at all for.
    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    // Same real fact the other Unmet-status tests establish: "design"'s
    // own test reference is genuinely stale against this repo's real git
    // history.
    assert!(harness.query_by_label("Unmet").is_some());
    assert!(harness.query_by_label("Unvalidated").is_none());
    assert!(
        harness
            .query_by_label_contains("Test procedure \"tests/smoke\": its reference is stale")
            .is_some()
    );
    // Still the same viewer, not bounced back to some other screen.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    );
}

#[test]
fn the_update_stale_references_button_appears_only_for_a_stale_reference_and_fixes_it() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Before validating, nothing is known to be stale yet — the button
    // must not show for an `Unvalidated` requirement.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });
    assert!(
        harness
            .query_by_label_contains("Update Stale References")
            .is_none()
    );

    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    // Same real, fact-checked outcome the other Unmet-status tests
    // establish — "design" is genuinely `Unmet` with a stale
    // `tests/smoke` reference now.
    assert!(harness.query_by_label("Unmet").is_some());
    // `\u{e094}` is `icons::UPDATE_STALE_REFERENCES` (`ARROWS_CLOCKWISE`) —
    // confirmed via the button's own accessible label, same "concatenated
    // icon + text" shape every other icon-plus-text button here has.
    harness
        .get_by_role_and_label(Role::Button, "\u{e094} Update Stale References")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });
    // The fix itself completes (clearing "pending") once
    // `RefreshStaleTestReferences` replies, but that reply only triggers
    // a *second* round trip — the re-fetched `GetEntryDetail` this file's
    // own `apply_refresh_stale_test_references_result` doc describes —
    // whose own completion briefly reintroduces a "pending" entry that
    // the `wait_until` above already waited out. What's left after that
    // is only the render catching up to the now-applied detail, so wait
    // on the actual condition (the button really gone) rather than
    // guessing a fixed number of settling frames.
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("Update Stale References")
            .is_none()
    });

    // Fixing it is itself an edit, so it demotes back to `Draft` same as
    // any other — but `RefreshStaleTestReferences` implicitly revalidates
    // on success (see `apply_refresh_stale_test_references_result`'s own
    // doc comment), so by the time it completes the project is already
    // re-`Validated` and the status line reads the real status straight
    // away, with no separate `Validate` call needed. The reference itself
    // is current now, so the remaining reason (if any) can no longer be
    // the stale reference — `test_project`'s results are all
    // `Incomplete`, not `Pass`, so it's still `Unmet`, just for a
    // different, real reason. The button itself is gone too — nothing
    // (still) known to be stale about the reference now that it's fixed.
    assert!(harness.query_by_label("Unmet").is_some());
    assert!(
        harness
            .query_by_label_contains("Update Stale References")
            .is_none()
    );
    // `apply_refresh_stale_test_references_result` marks `dirty` on
    // success, same as any other real edit.
    assert!(harness.query_by_label_contains("unsaved changes").is_some());

    // Re-validate: the reference is current now, so it can no longer be
    // reported as stale — `test_project`'s results are all `Incomplete`,
    // not `Pass`, so "design" is still `Unmet`, just for a different,
    // real reason (no current passing result) than before the fix, and
    // the button (nothing left for it to fix) stays gone.
    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    assert!(harness.query_by_label("Unmet").is_some());
    assert!(
        harness
            .query_by_label_contains("Test procedure \"tests/smoke\": its reference is stale")
            .is_none()
    );
    assert!(
        harness
            .query_by_label_contains("no current, passing result exists for it")
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("Update Stale References")
            .is_none()
    );
}

#[test]
fn the_update_stale_reference_button_appears_only_for_a_stale_result_and_fixes_it() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Same navigation `editing_an_existing_result_can_add_a_local_
    // attachment` uses to reach a result's viewer: open "design", then
    // follow its one result's link ("Design" is the result's own `title`,
    // distinct from the requirement's name "design").
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Design (Incomplete)")
            .is_some()
    });
    harness
        .get_by_role_and_label(Role::Label, "Design (Incomplete)")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Result").is_some()
    });

    // Before validating, nothing is known to be stale yet — the button
    // must not show for a result read against an `Unvalidated` project.
    assert!(
        harness
            .query_by_label_contains("Update Stale Reference")
            .is_none()
    );

    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    // `test_project`'s own fixture pins "design"'s result to commits
    // baked into its `result.ron` at fixture-authoring time — genuinely
    // stale against this repo's real, ever-advancing git history, same
    // "real fact, not a canned one" spirit as the requirement-level stale-
    // reference tests above.
    harness
        .get_by_role_and_label(Role::Button, "\u{e094} Update Stale Reference")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });
    // Same "the fix's own completion briefly reintroduces a `pending`
    // entry via the re-fetch that follows it" reasoning as the
    // requirement-level test above — wait on the button actually being
    // gone rather than a fixed frame count.
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("Update Stale Reference")
            .is_none()
    });

    // `apply_refresh_stale_result_reference_result` marks `dirty` on
    // success, same as any other real edit.
    assert!(harness.query_by_label_contains("unsaved changes").is_some());
}

#[test]
fn selecting_an_existing_requirement_opens_its_read_only_viewer() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // "design" is a real root-level requirement in `test_project`
    // (see `disk`'s own tests); its tree label includes the "\u{e32c}"
    // (MINUS_CIRCLE) unvalidated-status icon (`icons::status_icon`) since
    // this project hasn't been validated in this session.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    // The bare "Requirement" heading (not "Edit Requirement"/
    // "New Requirement") is the viewer's own — clicking a tree leaf lands
    // there by default, not straight into the editable form.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_none()
    );
    // The real title from `test_project/requirements/design/requirement.ron`,
    // shown as plain read-only text — no `Role::TextInput` for it, unlike
    // the editable form (see the next test).
    assert!(harness.query_by_label("Design").is_some());
    assert!(
        harness
            .query_by_role_and_label(Role::TextInput, "Design")
            .is_none()
    );
    // Only the toolbar's persistent "Save" exists — the form itself has
    // no Save/Cancel row while read-only, only "Edit".
    assert_eq!(
        harness
            .get_all_by_role_and_label(Role::Button, "Save")
            .count(),
        1
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Edit")
            .is_some()
    );

    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    });

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    );
    // `query_by_value` alone is ambiguous here (a `TextInput` node and its
    // own child `TextRun` both carry the same value, unlike labels, which
    // filter out the "labelled-by" node) — `get_all_by_value` sidesteps
    // that by not requiring uniqueness.
    assert!(harness.get_all_by_value("Design").next().is_some());
    // Two "Save" buttons now exist — the toolbar's persistent one and the
    // form's own (same ambiguity as the exit dialog's "Save" — see that
    // test's comment) — so this checks count, not a single unique query.
    assert_eq!(
        harness
            .get_all_by_role_and_label(Role::Button, "Save")
            .count(),
        2
    );
    // Renaming isn't supported once editing — the name field is disabled.
    let name_field = harness
        .get_all_by_value("design")
        .next()
        .expect("name field not found");
    assert!(name_field.accesskit_node().is_disabled());
}

#[test]
fn expanding_commit_history_shows_the_entrys_real_git_log() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // "design" is a real root-level requirement in `test_project`, a real
    // (checked-in) git repository — see `selecting_an_existing_requirement_
    // opens_its_read_only_viewer` above.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    // A `CollapsingHeader`'s own label reports as `Role::Button` — see
    // `the_tree_groups_leaves_under_requirements_and_test_procedures_
    // folders`'s own comment on this. Starts collapsed, so nothing's been
    // fetched yet; expanding it is what fires `Command::GetCommitLog` (see
    // `render_commit_log_section`'s own doc comment).
    harness
        .get_by_role_and_label(Role::Button, "Commit history")
        .click();
    harness.step();
    harness.step();

    // This is the real `harness()` (real `CoreHandle`, real `SystemGit`),
    // not a `FixedGit` fake — `requirements/design` is a real, checked-in
    // fixture in this repository's own git history, so a real `git log`
    // against it always finds at least one entry with a real (recent)
    // author date, never the "No commits yet." empty state a brand-new/
    // untracked path would show. `--date=short` formats each entry's date
    // as `YYYY-MM-DD`; any commit touching this repo is from the last few
    // years, so "202" is a safe (not over-fitted) substring to wait on.
    wait_until(&mut harness, |h| h.query_by_label_contains("202").is_some());
    assert!(harness.query_by_label("No commits yet.").is_none());
}

#[test]
fn clicking_a_commit_opens_its_file_list_then_a_files_colored_diff() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Commit history")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label_contains("202").is_some());

    // `ui.link()` reports as `Role::Label` in this accesskit conversion
    // (see `expanding_commit_history_shows_the_entrys_real_git_log`'s own
    // comment above), and its exact text (an 8-character hex hash) isn't
    // knowable ahead of time — locate the date label's row instead
    // (`render_commit_log_section`'s `egui::Grid` lays out hash, date,
    // subject per row, in that order, so the first `Role::Label` in the
    // same container as the date is that row's hash link) and click it.
    let date_label = harness.get_by_label_contains("202");
    let grid = date_label.parent().expect("commit history grid not found");
    grid.get_all_by_role(Role::Label)
        .next()
        .expect("commit hash link not found")
        .click();
    harness.step();
    harness.step();

    // The commit-files modal's own heading is "Commit <hash>" — same
    // "hash not knowable ahead of time" reasoning, so wait on the file-list
    // label it always renders once loaded instead.
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("Files changed").is_some()
    });
    assert!(harness.query_by_label("No files.").is_none());

    let files_label = harness.get_by_label_contains("Files changed");
    let file_list = files_label.parent().expect("commit files list not found");
    file_list
        .get_all_by_role(Role::Label)
        .nth(1) // skips the "Files changed (N):" label itself.
        .expect("changed file link not found")
        .click();
    harness.step();
    harness.step();

    // The diff modal's heading is "Diff: <path> @ <hash>" for a per-commit
    // diff (vs. plain "Diff: <path>" for the working-tree diff opened from
    // "Commit all changes") — the " @ " confirms `DiffDialogState::commit`
    // took the `Some` branch, i.e. this really is `Command::GetCommitFileDiff`'s
    // reply, not `Command::GetDiff`'s.
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("Diff: ").is_some()
    });
    assert!(harness.query_by_label_contains(" @ ").is_some());

    // The heading above renders synchronously the instant the dialog opens
    // — it proves the dialog opened, not that the diff arrived. Wait for
    // "Loading…" to clear (the async `Command::GetCommitFileDiff` reply
    // landing and `apply_diff_result` filling the dialog in) before
    // asserting on its contents, same reasoning as
    // `clicking_a_changed_file_opens_its_diff`'s own comment on this.
    wait_until(&mut harness, |h| h.query_by_label("Loading…").is_none());

    // Real, color-coded diff content from this repository's own history —
    // not the "No textual diff available" fallback a binary/no-op diff
    // would show.
    assert!(
        harness
            .query_by_label("No textual diff available (binary file, or no changes).")
            .is_none()
    );
}

#[test]
fn back_and_forward_toolbar_buttons_round_trip_two_real_selections() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    assert!(
        harness
            .get_by_role_and_label(Role::Button, "Back")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        harness
            .get_by_role_and_label(Role::Button, "Forward")
            .accesskit_node()
            .is_disabled()
    );

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    // Lands on the read-only viewer — "Design" shows as a plain
    // label, not a `TextInput` value, until "Edit" is clicked (see
    // `selecting_an_existing_requirement_opens_its_read_only_viewer`).
    wait_until(&mut harness, |h| h.query_by_label("Design").is_some());
    // Opening a project lands on its own root page first — that's
    // history stop #0 — so this first real leaf selection already has
    // somewhere to go Back to.
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Back")
            .accesskit_node()
            .is_disabled()
    );

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} external")
        .click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("External").is_some());
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Back")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        harness
            .get_by_role_and_label(Role::Button, "Forward")
            .accesskit_node()
            .is_disabled()
    );

    harness.get_by_role_and_label(Role::Button, "Back").click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Design").is_some());
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Forward")
            .accesskit_node()
            .is_disabled()
    );

    harness
        .get_by_role_and_label(Role::Button, "Forward")
        .click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("External").is_some());
}

#[test]
fn the_edit_buttons_navigation_registers_with_back_and_forward() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Edit").is_some()
    });
    // Opening a project lands on its own root page first — that's
    // history stop #0 — so this first real leaf selection already has
    // somewhere to go Back to.
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Back")
            .accesskit_node()
            .is_disabled()
    );

    // Clicking "Edit" is itself a navigation — per the user's own
    // request, it must register with Back/Forward the same as clicking a
    // different tree leaf does.
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    });
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Back")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        harness
            .get_by_role_and_label(Role::Button, "Forward")
            .accesskit_node()
            .is_disabled()
    );

    harness.get_by_role_and_label(Role::Button, "Back").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_none()
    );
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Forward")
            .accesskit_node()
            .is_disabled()
    );

    harness
        .get_by_role_and_label(Role::Button, "Forward")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    });
}

#[test]
fn saving_an_edit_to_an_existing_requirement_closes_the_form_and_marks_dirty() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");
    assert!(harness.query_by_label("saved").is_some());

    let title_field = harness
        .get_all_by_value("Design")
        .next()
        .expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    // `test_project`'s "design" fixture has empty requirement text on
    // disk — the same non-empty-main-text rule the create form enforces
    // now applies to edits too, so it has to be filled in here for Save
    // to succeed.
    let requirement_text_field = harness
        .get_all_by_role(Role::MultilineTextInput)
        .next()
        .expect("requirement text field not found");
    requirement_text_field.focus();
    requirement_text_field.type_text("Some requirement text.");
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

    wait_until(&mut harness, |h| {
        h.query_by_label("\u{e18a} unsaved changes").is_some()
    });

    // A successful edit-mode Save now navigates back to the entry's
    // read-only viewer (per `apply_update_result`), same as Cancel does —
    // it doesn't leave the edit form sitting open.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_none()
    );
    // Read-only mode renders the title as a plain label, not a TextEdit —
    // the saved value shows up as its own label rather than a widget value.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Design (edited)")
            .is_some()
    );
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn navigating_away_from_an_edited_field_prompts_and_cancel_stays_put() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");

    let title_field = harness
        .get_all_by_value("Design")
        .next()
        .expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    // A different tree leaf — this must not navigate away immediately.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} external")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_some()
    );
    // Still on "design"'s edit form, untouched.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    );
    assert!(harness.get_all_by_value("Design (edited)").next().is_some());

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

    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_none()
    );
    // Cancelling the prompt leaves the edit exactly as it was — neither
    // navigated away nor itself discarded (that's what the form's own
    // Cancel button is for, a separate, deliberately unprompted action).
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    );
    assert!(harness.get_all_by_value("Design (edited)").next().is_some());
}

#[test]
fn navigating_away_from_an_edited_field_prompts_and_continue_discards_and_navigates() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");

    let title_field = harness
        .get_all_by_value("Design")
        .next()
        .expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} external")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_some()
    );

    harness
        .get_by_role_and_label(Role::Button, "Continue")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_none()
    );
    // Landed on "external"'s own viewer — the click that was interrupted
    // actually went through once confirmed.
    wait_until(&mut harness, |h| h.query_by_label("External").is_some());
    assert!(harness.query_by_label("External").is_some());
}

#[test]
fn navigating_away_from_an_untouched_edit_form_does_not_prompt() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    // Into the editable form, but nothing typed — Edit alone doesn't mark
    // anything `edited`.
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} external")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_none()
    );
    wait_until(&mut harness, |h| h.query_by_label("External").is_some());
    assert!(harness.query_by_label("External").is_some());
}

#[test]
fn the_forms_own_cancel_button_discards_without_prompting() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");

    let title_field = harness
        .get_all_by_value("Design")
        .next()
        .expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    // The form's own Cancel — the second "Cancel" in tree order isn't a
    // concern here (no other "Cancel" exists with a project loaded and
    // no other dialog open), unlike the ambiguous "Save"/toolbar cases
    // elsewhere in this file.
    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    harness.step();
    harness.step();

    // No prompt at all — Cancel is already the explicit "discard" action,
    // see `PendingNavigation`'s own doc comment on why it's excluded from
    // this gate.
    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_none()
    );
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_none()
    );
}

#[test]
fn back_and_forward_are_gated_on_unsaved_form_edits_too() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("Design").is_some());
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} external")
        .click();
    harness.step();
    wait_until(&mut harness, |h| h.query_by_label("External").is_some());

    // Now edit "external", then try Back — it should prompt rather than
    // silently navigating away from the unsaved edit. Nav history at
    // this point: design(View), external(View), external(Edit) —
    // Back moves one step, to external(View), not all the way back to
    // design (see "Forwards/backwards navigation" — clicking Edit is
    // its own navigation step).
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    });
    let title_field = harness
        .get_all_by_value("External")
        .next()
        .expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Back").click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_some()
    );

    harness
        .get_by_role_and_label(Role::Button, "Continue")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_none()
    );
    assert!(harness.get_all_by_value("External").next().is_some());
}

#[test]
fn new_requirement_button_is_gated_on_unsaved_form_edits() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");

    let title_field = harness
        .get_all_by_value("Design")
        .next()
        .expect("title field not found");
    title_field.focus();
    title_field.type_text(" (edited)");
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_some()
    );
    // Still editing "design" — the click didn't go through yet.
    assert!(harness.get_all_by_value("Design (edited)").next().is_some());

    harness
        .get_by_role_and_label(Role::Button, "Continue")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Requirement")
            .is_some()
    );
}

#[test]
fn a_requirements_dependency_can_be_viewed_removed_and_a_new_one_added() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Viewer first — "integration" has one real dependency in
    // `test_project` (on "design", see `requirement.ron`); its brief
    // summary text (`DependencyDraft::brief`, no pinned commit — that's
    // implementation detail in the read-only viewer, see that method's
    // own doc comment) should be visible as a clickable link, no
    // `TextInput`/Remove button.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} integration")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label("requirements/design").is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Remove")
            .is_none()
    );

    // Into the editable form — the same dependency is now editable, with
    // a "Remove" button (and no local attachments on "integration" to add
    // a second one — see `adding_a_local_attachment_...`'s own comment on
    // this same fixture entry). "integration" also has two test
    // references (`tests/smoke`, `tests/contract` — see `requirement.ron`),
    // each with their own "Remove" button: 1 dependency + 2 test
    // references = 3.
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    });
    assert_eq!(
        harness
            .get_all_by_role_and_label(Role::Button, "Remove")
            .count(),
        3
    );

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
    // The two test-reference rows' own "Remove" buttons are untouched by
    // this — only the dependency's own row was removed.
    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "Remove")
            .count(),
        2
    );

    // The composer's fields stay hidden until "Add dependency" is
    // clicked once to reveal them.
    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();
    // Opening the composer now opens a modal dialog (a separate
    // `egui::Modal` overlay), which needs one more `step()` to settle
    // before its own fields are reliably interactable — same rule as
    // `close_button_closes_the_attachments_dialog` (see this file's
    // module doc comment).
    harness.step();

    // The modal renders its own heading plus exactly two `TextInput`s
    // (path, then commit) — scoping the query to the heading's parent
    // container, rather than indexing into the whole form's flat
    // `TextInput` list, stays correct regardless of how many other
    // fields (existing rows, the always-present attachment field) sit
    // elsewhere in the accessibility tree.
    fn dependency_composer_field<'h>(
        harness: &'h Harness<'_, GuiApp>,
        idx: usize,
    ) -> egui_kittest::Node<'h> {
        harness
            .get_by_role_and_label(Role::Label, "Add Dependency")
            .parent()
            .expect("dependency composer modal container not found")
            .get_all_by_role(Role::TextInput)
            .nth(idx)
            .expect("dependency composer field not found")
    }
    dependency_composer_field(&harness, 0).focus();
    dependency_composer_field(&harness, 0).type_text("/requirements/external");
    harness.step();
    dependency_composer_field(&harness, 1).focus();
    dependency_composer_field(&harness, 1).type_text("newcommit");
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    // Pushed onto the (now-editable, still edit-mode) list — its own
    // Remove button confirms a real new row exists, and its value is
    // still typed-in plain fields at this point, not the read-only
    // summary string (that only renders once `read_only` — see the
    // viewer assertion below, after Save returns to it). Plus the two
    // untouched test-reference rows' own Remove buttons: 3 total.
    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "Remove")
            .count(),
        3
    );
    assert!(
        harness
            .get_all_by_value("/requirements/external")
            .next()
            .is_some()
    );
    assert!(harness.get_all_by_value("newcommit").next().is_some());

    // `test_project`'s "integration" fixture has empty requirement text
    // on disk — the same non-empty-main-text rule the create form
    // enforces now applies to edits too, so it has to be filled in here
    // for Save to succeed.
    let requirement_text_field = harness
        .get_all_by_role(Role::MultilineTextInput)
        .next()
        .expect("requirement text field not found");
    requirement_text_field.focus();
    requirement_text_field.type_text("Some requirement text.");
    harness.step();

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

    wait_until(&mut harness, |h| {
        h.query_by_label("\u{e18a} unsaved changes").is_some()
    });
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn a_submodule_dependencys_link_navigates_to_that_module() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // "design" (project root) depends on "beta" here rather than "alpha" —
    // `modules/alpha/requirements/spec` already depends back on "design"
    // (see its own `requirement.ron`), so naming "alpha" would create a
    // dependency cycle; "beta" has no requirements of its own, so nothing
    // to cycle back through. This only exercises `UpdateRequirement`
    // against the in-memory draft (never `Validate`/`Save` to disk), so —
    // like the dependency add/remove test above — it's safe to run
    // against the real `test_project` fixture rather than a scratch copy.
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");

    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();
    harness.step();

    fn composer<'h>(harness: &'h Harness<'_, GuiApp>) -> egui_kittest::Node<'h> {
        harness
            .get_by_role_and_label(Role::Label, "Add Dependency")
            .parent()
            .expect("dependency composer modal container not found")
    }
    composer(&harness)
        .get_by_role_and_label(Role::RadioButton, "Submodule")
        .click_accesskit();
    harness.step();

    composer(&harness)
        .get_by_value("(choose a submodule)")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_all_by_role_and_label(Role::Button, "beta")
        .last()
        .expect("submodule dropdown option 'beta' not found")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    // "design"'s own requirement text is empty on disk (same rule the
    // dependency add/remove test above already documents) — fill it in
    // so Save doesn't reject an empty main text.
    let requirement_text_field = harness
        .get_all_by_role(Role::MultilineTextInput)
        .next()
        .expect("requirement text field not found");
    requirement_text_field.focus();
    requirement_text_field.type_text("Some requirement text.");
    harness.step();

    harness
        .get_all_by_role_and_label(Role::Button, "Save")
        .nth(1)
        .expect("form Save button not found")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    // Back in the read-only viewer: the new dependency shows as a
    // "Submodule:" label next to a clickable link showing the submodule's
    // fully qualified, `modules/`-prefixed, leading-`/` path (not
    // `DependencyDraft::brief`'s "Submodule: beta" form, which would be
    // redundant with the leading label — see `render_requirement_form`'s
    // own comment on this). "design" lives at the project root, so
    // "beta"'s fully qualified path is "/modules/beta" here, matching the
    // same `modules/`-prefixed convention `LocalRequirement` dependency
    // links and every other path in this app use. Scoped to the row's own
    // container (`.parent()` of the "Submodule:" label) rather than a bare
    // `harness.get_by_label("/modules/beta")` since that'd still be
    // unambiguous here, but matches the pattern the rest of this test
    // follows. `Role::Label` rather than `Role::Link` — a real `ui.link()`
    // reports as `Role::Label` in this accesskit conversion, same as the
    // pre-existing `LocalRequirement` dependency link above (confirmed
    // empirically: neither shows up under `Role::Link`).
    let dependency_row = harness
        .get_by_role_and_label(Role::Label, "Submodule:")
        .parent()
        .expect("submodule dependency row not found");
    dependency_row.get_by_label("/modules/beta").click();
    harness.step();

    // Clicking it opens "beta"'s own module page, the same destination
    // its tree row's "Set as current module" button would — modules have
    // no `EntryPath` of their own, so this goes through `select_module`
    // rather than the leaf-navigation `select` every other dependency
    // link above uses.
    wait_until(&mut harness, |h| {
        h.query_all_by_label("Module: beta").next().is_some()
    });
    assert!(harness.query_all_by_label("Module: beta").next().is_some());
}

#[test]
fn adding_a_named_submodule_dependency_offers_the_current_modules_own_submodules() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // "design" lives at the project root, whose own direct submodules are
    // "alpha" and "beta" (see
    // `requirement_form_dependency_picker_scopes_by_this_module_and_submodules`'s
    // own comment on this fixture's module layout) — "alpha_child" (nested
    // under "alpha") must NOT be offered, since a named-submodule
    // dependency only ever names a *direct* child.
    open_leaf_for_editing(&mut harness, "\u{e32c} design", "Edit Requirement");

    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    fn composer<'h>(harness: &'h Harness<'_, GuiApp>) -> egui_kittest::Node<'h> {
        harness
            .get_by_role_and_label(Role::Label, "Add Dependency")
            .parent()
            .expect("dependency composer modal container not found")
    }
    composer(&harness)
        .get_by_role_and_label(Role::RadioButton, "Submodule")
        .click_accesskit();
    harness.step();

    // The sidebar tree (expanded by `open_test_project`) already has its
    // own "alpha"/"alpha_child"/"beta" module-row buttons, so the popup's
    // own options can't be told apart from those by label alone — count
    // before and after opening the popup instead. "alpha_child" (nested
    // under "alpha", not a direct child of the project root) must not
    // gain a second entry; "alpha"/"beta" (the root's own direct
    // submodules) must.
    let alpha_before = harness
        .query_all_by_role_and_label(Role::Button, "alpha")
        .count();
    let alpha_child_before = harness
        .query_all_by_role_and_label(Role::Button, "alpha_child")
        .count();
    let beta_before = harness
        .query_all_by_role_and_label(Role::Button, "beta")
        .count();

    composer(&harness)
        .get_by_value("(choose a submodule)")
        .click_accesskit();
    harness.step();
    harness.step();

    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "alpha")
            .count(),
        alpha_before + 1
    );
    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "beta")
            .count(),
        beta_before + 1
    );
    assert_eq!(
        harness
            .query_all_by_role_and_label(Role::Button, "alpha_child")
            .count(),
        alpha_child_before
    );

    // The popup's own entry is whichever "alpha" button wasn't there
    // before it opened — the newly-added one is last in paint order.
    harness
        .get_all_by_role_and_label(Role::Button, "alpha")
        .last()
        .expect("submodule dropdown option 'alpha' not found")
        .click_accesskit();
    harness.step();
    harness.step();

    assert!(composer(&harness).query_by_value("alpha").is_some());

    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    // Back in edit mode with the row added but not yet saved — the
    // freshly-added dependency isn't rendered as a summary string until
    // Save returns to the read-only viewer, so check its kind dropdown
    // instead, mirroring how the existing-row assertions elsewhere in this
    // file confirm a field's live value rather than a rendered summary.
    assert!(harness.get_all_by_value("Submodule").next().is_some());
}

#[test]
fn an_existing_dependencys_own_pick_and_auto_buttons_update_that_row() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} integration", "Edit Requirement");

    // "integration" has one real dependency in `test_project` (on
    // "design", commit `9f8e7d6c5b4a3928170695847362514031201f0e` —
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
    assert_eq!(initial_commit, "9f8e7d6c5b4a3928170695847362514031201f0e");

    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("existing dependency's Pick button not found")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a requirement")
            .is_some()
    );

    // "external" is a different real root-level requirement —
    // switching the existing row to point at it (rather than re-picking
    // "design") makes the follow-up Auto fetch below meaningfully
    // check something changed, not just that the field still happened to
    // hold a hex string.
    harness
        .get_all_by_label("external")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .get_all_by_value("/requirements/external")
            .next()
            .is_some()
    );
    // Still the *existing* row that changed, not a second row added — the
    // dependency's own "Remove" button plus the two untouched test-reference
    // rows' own Remove buttons: 3 total.
    assert_eq!(
        harness
            .get_all_by_role_and_label(Role::Button, "Remove")
            .count(),
        3
    );

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
            .is_some_and(|value| {
                value != initial_commit
                    && value.len() == 40
                    && value.chars().all(|c| c.is_ascii_hexdigit())
            })
    });
}

#[test]
fn requirement_form_dependency_path_picker_fills_the_field() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Requirement")
            .is_some()
    );

    // The composer's fields stay hidden until "Add dependency" is
    // clicked once to reveal them.
    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    // The "Add dependency" composer's default `Local` variant carries its
    // own path picker now — same modal mechanics as the Result form's own
    // pickers (see `result_form_requirement_path_picker_fills_the_field`).
    // The composer itself now renders inside its own `egui::Modal`, so its
    // "Pick…" button is scoped by walking up from the composer's own
    // heading rather than indexing into a flat, whole-document list.
    harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("dependency composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step(); // let the modal settle, same as the Result form's own test.

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a requirement")
            .is_some()
    );

    // "design" is a real root-level requirement in `test_project`.
    // As with the Result form's own pickers, the tree's own leaf button
    // for it also matches by label and sorts first in tree order, so
    // `.last()` is what actually reaches the modal's own row.
    harness
        .get_all_by_label("design")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .get_all_by_value("/requirements/design")
            .next()
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Requirement")
            .is_some()
    );
}

#[test]
fn requirement_form_dependency_auto_button_fetches_the_commit() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();

    // The composer's fields stay hidden until "Add dependency" is
    // clicked once to reveal them.
    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("dependency composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step();
    harness
        .get_all_by_label("design")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    // The dependency composer's own "Auto" button, scoped the same way as
    // its "Pick…" button above — the modal's own container has exactly
    // one "Auto" button (the composer's own; there are no existing
    // dependency rows in this fresh create-mode form to add another).
    // Clicking it round-trips through the real `CoreHandle`'s actor, which
    // shells out to real `git` against `test_project` (a real, tracked
    // directory in this very repo — see `open_test_project`'s own doc
    // comment) to resolve `requirements/design`'s latest commit.
    harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Auto")
        .next()
        .expect("dependency composer Auto button not found")
        .click();
    harness.step();

    // The commit field is the modal's own second `Role::TextInput` (path,
    // then commit) — scoped via the composer's own heading rather than a
    // flat whole-document index, same technique as
    // `a_requirements_dependency_can_be_viewed_removed_and_a_new_one_added`.
    // Asserted by shape (40 hex characters — a real commit hash), not a
    // specific value, since the actual commit depends on this repo's own
    // history rather than anything `test_project`'s fixture data pins in
    // place.
    wait_until(&mut harness, |h| {
        h.get_by_role_and_label(Role::Label, "Add Dependency")
            .parent()
            .expect("dependency composer modal container not found")
            .get_all_by_role(Role::TextInput)
            .nth(1)
            .and_then(|field| field.value())
            .is_some_and(|value| value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()))
    });
}

#[test]
fn requirement_form_remote_dependency_auto_button_fetches_the_commit() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();

    // The composer's fields stay hidden until "Add dependency" is
    // clicked once to reveal them.
    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    // Switches the "Add dependency" composer from its default `Local`
    // variant to `Remote` — a real, separate code path in
    // `render_dependency_fields`/`dependency_commit_auto_clicked`
    // (`AutoCommitKind::Remote`, resolved via `RemoteGit::commit_for_remote`
    // rather than `Git::commit_for_path`), untested until now (the
    // existing Auto tests only ever exercise the `Local` variant).
    harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_by_role_and_label(Role::RadioButton, "Remote")
        .click_accesskit();
    harness.step();
    harness.step();

    // The modal's own URL(0)/Path(1)/Commit(2) fields, scoped via the
    // composer's own heading rather than a flat whole-document index —
    // same technique as the `Local` variant's own test.
    let url_field = harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("url field not found");
    url_field.focus();
    // This repo's own root, addressed as a `file://` remote — a real git
    // repository `commit_for_remote` can actually clone/inspect without
    // any network access, same trick `syscalls`' own tests use (see
    // `file_url` below).
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    url_field.type_text(&file_url(&repo_root));
    harness.step();

    let path_field = harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role(Role::TextInput)
        .nth(1)
        .expect("path field not found");
    path_field.focus();
    path_field.type_text("test_project/requirements/design");
    harness.step();

    // The dependency composer's own "Auto" button, scoped the same way as
    // the `Local` variant's own test.
    harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Auto")
        .next()
        .expect("dependency composer Auto button not found")
        .click();
    harness.step();

    // Same "assert by shape, not by exact value" reasoning as the `Local`
    // variant's own test — the real commit depends on this repo's own
    // history.
    wait_until(&mut harness, |h| {
        h.get_by_role_and_label(Role::Label, "Add Dependency")
            .parent()
            .expect("dependency composer modal container not found")
            .get_all_by_role(Role::TextInput)
            .nth(2)
            .and_then(|field| field.value())
            .is_some_and(|value| value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()))
    });
}

#[test]
fn requirement_form_dependency_composer_pick_auto_populates_the_commit() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();

    // The composer's fields stay hidden until "Add dependency" is
    // clicked once to reveal them.
    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("dependency composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step();
    harness
        .get_all_by_label("design")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    // No "Auto" click here — picking a target now implies it, same as
    // `path_picker_dialog_selected`'s `DependencySlot::Existing` case
    // (see `an_existing_dependencys_own_pick_and_auto_buttons_update_that_row`)
    // extended to the composer's own not-yet-added row.
    wait_until(&mut harness, |h| {
        h.get_by_role_and_label(Role::Label, "Add Dependency")
            .parent()
            .expect("dependency composer modal container not found")
            .get_all_by_role(Role::TextInput)
            .nth(1)
            .and_then(|field| field.value())
            .is_some_and(|value| value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()))
    });
}

#[test]
fn requirement_form_test_reference_composer_pick_auto_populates_the_commit() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Requirement")
        .click();
    harness.step();

    // The composer's fields stay hidden until "Add test procedure" is
    // clicked once to reveal them.
    harness
        .get_by_role_and_label(Role::Button, "Add test procedure")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Label, "Add Test Procedure")
        .parent()
        .expect("test reference composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("test reference composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step();

    // "smoke" is a real test leaf in `test_project` (see `tests/smoke`
    // in `requirement.ron`'s fixtures).
    harness
        .get_all_by_label("smoke")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    // Same "Pick… implies Auto" shortcut as the dependency composer's own
    // test above, exercising `TestRefSlot::New` through
    // `test_ref_commit_auto_clicked` instead.
    wait_until(&mut harness, |h| {
        h.get_by_role_and_label(Role::Label, "Add Test Procedure")
            .parent()
            .expect("test reference composer modal container not found")
            .get_all_by_role(Role::TextInput)
            .nth(1)
            .and_then(|field| field.value())
            .is_some_and(|value| value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()))
    });
}

#[test]
fn path_picker_dialog_cancel_closes_it_without_changing_the_field() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Result")
        .click();
    harness.step();
    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("requirement path picker not found")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a requirement")
            .is_some()
    );

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

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a requirement")
            .is_none()
    );
    // The requirement-path field is untouched — still empty, not filled
    // in by a cancelled picker. `query_by_value`, not `get_all_by_value`
    // (which panics on zero matches — exactly the case being asserted).
    assert!(harness.query_by_value("/requirements/design").is_none());
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Result")
            .is_some()
    );
}

#[test]
fn path_picker_dialog_shows_no_matches_for_an_unmatched_filter() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Result")
        .click();
    harness.step();
    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("requirement path picker not found")
        .click();
    harness.step();
    harness.step();

    assert!(harness.query_by_label("No matches.").is_none());

    // No requirement in `test_project` has "zzz" anywhere in its
    // fully-qualified path, so this should empty the list out entirely —
    // `render_path_picker_dialog`'s own fallback text for that case.
    let filter_field = harness
        .get_all_by_role(Role::TextInput)
        .last()
        .expect("filter field not found");
    filter_field.focus();
    filter_field.type_text("zzz_no_such_entry");
    harness.step();

    assert!(harness.query_by_label("No matches.").is_some());
    // Confirms the list itself is actually empty, not just coincidentally
    // missing that one label — no node bare-labelled "design" remains at
    // all: the modal's own row is filtered out, the tree's own leaf
    // button carries a status-glyph prefix ("\u{e32c} design", not a bare
    // match), and results (which would otherwise nest a bare-named row
    // under it) don't get their own tree row any more.
    assert_eq!(harness.query_all_by_label("design").count(), 0);
}

#[test]
fn adding_a_local_attachment_to_an_existing_requirement_appears_in_the_list() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    open_leaf_for_editing(&mut harness, "\u{e32c} integration", "Edit Requirement");

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
    // title(3), then — "integration" has one real dependency in
    // `test_project` (on "design") — its own path(4)/commit(5)
    // fields (the "Add dependency" composer's own fields stay hidden
    // until "Add dependency" is clicked, so they don't appear here), then
    // — "integration" also has two real test references (`tests/smoke`,
    // `tests/contract` — see `requirement.ron`) — their own path(6)/
    // commit(7) and path(8)/commit(9) fields (the "Add test procedure"
    // composer's fields are likewise hidden by default), then — since
    // this form is in edit mode — the local-attachment path field(10).
    // Fragile to reordering singleline fields specifically, which is why
    // this comment exists.
    let attachment_path_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(10)
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
    harness
        .get_by_role_and_label(Role::Button, "Add")
        .click_accesskit();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_label("interaction_test_attachment.md").is_some()
    });

    assert!(
        harness
            .query_by_label("interaction_test_attachment.md")
            .is_some()
    );
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn creating_a_result_from_the_requirement_views_empty_state_opens_a_modal_and_creates_it() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    // "Create new result" only shows once the requirement has at least one
    // test procedure of its own (see `CreateResultDialogState`'s own doc
    // comment) — `create_scratch_requirement` leaves it with none, so add
    // one first via the real "Add test procedure" composer, same flow as
    // `requirement_form_test_reference_composer_pick_auto_populates_the_commit`,
    // but completed all the way through (that test stops after the Pick
    // step) and saved for real.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Edit").is_some()
    });
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Add test procedure")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Label, "Add Test Procedure")
        .parent()
        .expect("test reference composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("test reference composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step();

    // "smoke" is a real test leaf in `test_project` (see `tests/smoke` in
    // `requirement.ron`'s fixtures).
    harness
        .get_all_by_label("smoke")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    // The composer's own confirm button — the reveal button of the same
    // name is gone while the composer is open, so this is unambiguous.
    harness
        .get_by_role_and_label(Role::Button, "Add test procedure")
        .click_accesskit();
    harness.step();

    // Persist the new test reference for real — the empty-results state's
    // "Create new result" button reads `form.tests`, which only matters
    // once this requirement actually has a saved test procedure, not just
    // an in-progress form edit.
    harness
        .get_all_by_role_and_label(Role::Button, "Save")
        .nth(1)
        .expect("form Save button not found")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    // Back in the read-only viewer — "Create new result" doesn't mutate
    // the requirement itself (it opens the Create Result dialog, which
    // creates a separate Result entity), so it's shown here too, unlike
    // Local attachments' own Add/Remove controls.
    assert!(
        harness
            .query_by_label("No results reference this requirement yet.")
            .is_some()
    );
    harness
        .get_by_role_and_label(Role::Button, "Create new result")
        .click_accesskit();
    harness.step();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "New Result")
            .is_some()
    });

    let fields: Vec<_> = harness
        .get_by_role_and_label(Role::Label, "New Result")
        .parent()
        .expect("create-result modal container not found")
        .get_all_by_role(Role::TextInput)
        .collect();
    let identifier_field = fields.first().expect("result identifier field not found");
    let title_field = fields.get(1).expect("result title field not found");

    // Prefilled as `<today> <test name>` (see
    // `GuiApp::create_result_clicked`/`default_result_name`) — checked
    // structurally (a `YYYY-MM-DD` date, then the test name) rather than
    // against a hardcoded date, which would go stale.
    let prefilled = identifier_field
        .value()
        .expect("identifier field has no value");
    let (date_part, rest) = prefilled
        .split_once(' ')
        .expect("identifier missing date/name separator");
    assert_eq!(date_part.len(), 10);
    assert!(date_part.char_indices().all(|(i, c)| if i == 4 || i == 7 {
        c == '-'
    } else {
        c.is_ascii_digit()
    }));
    assert_eq!(rest, "smoke");

    // "Generate title from identifier" is checked by default, same as
    // the Duplicate/Recreate dialogs' own "Regenerate title" checkbox —
    // and, since it's checked, the title field already mirrors a
    // title-cased version of the identifier and is disabled rather than
    // independently editable.
    assert_eq!(
        harness
            .get_by_role_and_label(Role::CheckBox, "Generate title from identifier")
            .accesskit_node()
            .toggled(),
        Some(Toggled::True)
    );
    assert_eq!(
        title_field.value().as_deref(),
        Some(logical::draft::title_case_from_name(&prefilled).as_str())
    );
    assert!(title_field.accesskit_node().is_disabled());

    identifier_field.focus();
    identifier_field.type_text(" (edited)");
    harness.step();

    // Still synced live after editing the identifier, not just at open.
    let fields: Vec<_> = harness
        .get_by_role_and_label(Role::Label, "New Result")
        .parent()
        .expect("create-result modal container not found")
        .get_all_by_role(Role::TextInput)
        .collect();
    let identifier_field = fields.first().expect("result identifier field not found");
    let title_field = fields.get(1).expect("result title field not found");
    let edited_identifier = identifier_field
        .value()
        .expect("identifier field has no value");
    assert!(edited_identifier.ends_with(" (edited)"));
    assert_eq!(
        title_field.value().as_deref(),
        Some(logical::draft::title_case_from_name(&edited_identifier).as_str())
    );

    harness
        .get_by_role_and_label(Role::Button, "Create result")
        .click();
    harness.step();

    // The confirm click sends a real `AddResult` and, on success, navigates
    // straight to the new result's own read-only viewer — same "go look at
    // what you just created" reasoning as the Duplicate prompt.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Result").is_some()
    });
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn switching_the_create_result_dialogs_test_picker_updates_the_prefilled_identifier() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // "integration" is a real root-level requirement in `test_project`
    // with two test references of its own — `tests/smoke` and
    // `tests/contract` (see `test_project/requirements/integration/
    // requirement.ron`) — so switching the dialog's test picker is
    // actually meaningful here, unlike "design"/"external" which only
    // have one test each.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} integration")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Create new result")
        .click_accesskit();
    harness.step();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "New Result")
            .is_some()
    });

    let identifier_field = harness
        .get_by_role_and_label(Role::Label, "New Result")
        .parent()
        .expect("create-result modal container not found")
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("result identifier field not found");
    // Prefilled from the first test reference, "tests/smoke" — same
    // `<today> <test name>` shape asserted in
    // `creating_a_result_from_the_requirement_views_empty_state_opens_a_modal_and_creates_it`.
    let prefilled = identifier_field
        .value()
        .expect("identifier field has no value");
    assert!(
        prefilled.ends_with("smoke"),
        "expected identifier prefilled from tests/smoke, got {prefilled:?}"
    );

    // Switch the test picker to the requirement's other test procedure —
    // the `ComboBox` trigger reports its selected text ("tests/smoke") as
    // an AccessKit value, same convention as the theme selector's own
    // `ComboBox` test.
    harness
        .get_all_by_role(Role::ComboBox)
        .find(|node| node.value().as_deref() == Some("tests/smoke"))
        .expect("test picker combo box not found")
        .click();
    harness.step();
    harness.step(); // let the popup settle.

    harness
        .get_by_role_and_label(Role::Button, "tests/contract")
        .click();
    harness.step();
    harness.step();

    // The identifier must now follow the newly selected test, not stay
    // stuck on the one it was prefilled from — this is the actual bug fix
    // under test: previously switching the dropdown left the identifier
    // (and, transitively, the title still following it) referencing the
    // old test.
    let identifier_field = harness
        .get_by_role_and_label(Role::Label, "New Result")
        .parent()
        .expect("create-result modal container not found")
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("result identifier field not found");
    let updated = identifier_field
        .value()
        .expect("identifier field has no value");
    assert!(
        updated.ends_with("contract"),
        "expected identifier to follow the newly selected tests/contract, got {updated:?}"
    );
}

#[test]
fn creating_a_result_with_unsaved_requirement_edits_prompts_before_discarding_them() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    create_scratch_requirement(&mut harness);

    // Same "Add test procedure" flow as
    // `creating_a_result_from_the_requirement_views_empty_state_opens_a_modal_and_creates_it`,
    // but — unlike that test — the outer form's own Save is deliberately
    // *not* clicked afterward, so the new test reference stays a local,
    // unsubmitted `form.tests` edit (`form.edited == true`) rather than
    // something actually persisted to the requirement on disk. That's the
    // state this bug was reported in: creating a result while the
    // requirement form itself still has edits pending.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Edit").is_some()
    });
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Add test procedure")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Label, "Add Test Procedure")
        .parent()
        .expect("test reference composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("test reference composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_all_by_label("smoke")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Button, "Add test procedure")
        .click_accesskit();
    harness.step();

    // The requirement form's own Save is never clicked — `form.tests` now
    // holds an unsubmitted edit, and "Create new result" reads that local
    // state (see `GuiApp::create_result_clicked`), so it's available here
    // exactly as it would be for a saved test reference.
    harness
        .get_by_role_and_label(Role::Button, "Create new result")
        .click_accesskit();
    harness.step();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "New Result")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Create result")
        .click();
    harness.step();
    harness.step();

    // The `AddResult` succeeds, but landing straight on the new result's
    // read-only viewer (the usual "go look at what you just created" flow)
    // would silently replace `self.editor` and discard the still-unsaved
    // test reference sitting in the requirement form — the exact bug
    // reported. So instead of navigating immediately, this must fall back
    // to the same "unsaved changes" gate every other navigation that
    // would replace `self.editor` already goes through (see
    // `editor_has_unsaved_edits` call sites in `view.rs`).
    wait_until(&mut harness, |h| {
        h.query_by_label("This form has unsaved changes. Continue and lose them?")
            .is_some()
    });
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Result")
            .is_none()
    );
    // The requirement form underneath is still showing, edits intact —
    // not silently swapped out for the result viewer.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    );

    // Cancelling the prompt keeps the requirement form open with its
    // unsaved test reference still there, rather than losing it. Scoped
    // to the unsaved-changes modal's own container — the requirement
    // form underneath has a "Cancel" button of its own too (currently
    // disabled, since a request is still pending), so an unscoped query
    // would be ambiguous.
    harness
        .get_by_label("This form has unsaved changes. Continue and lose them?")
        .parent()
        .expect("unsaved-changes modal container not found")
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Edit Requirement")
            .is_some()
    );
    // The test reference's own "Path:" field (a plain `TextEdit`, so its
    // text is an AccessKit *value*, not a label — see
    // `render_test_ref_fields`) still holds "/tests/smoke" (the absolute
    // reference path the Pick flow filled in): the unsaved edit survived,
    // it wasn't silently dropped.
    assert!(
        harness
            .get_all_by_role(Role::TextInput)
            .any(|node| node.value().as_deref() == Some("/tests/smoke"))
    );
}

#[test]
fn create_new_result_button_stays_present_once_a_requirement_already_has_a_result() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // "design" is a real root-level requirement in `test_project` with
    // both a test reference and an existing result already on disk (see
    // `test_project/requirements/design/`) — the case the empty-results
    // shortcut used to hide "Create new result" for, since the button only
    // rendered inside the `form.results.is_empty()` branch. It must stay
    // reachable even once results already exist, so a second/third result
    // is exactly as easy to add as the first.
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Requirement")
            .is_some()
    });

    assert!(
        harness
            .query_by_label("No results reference this requirement yet.")
            .is_none()
    );
    assert!(
        !harness
            .get_by_role_and_label(Role::Button, "Create new result")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn opening_a_project_defaults_to_the_root_view_page() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    assert!(harness.query_by_label("Project: Test Project").is_some());
    // Recursive totals across the whole tree, not just root-level counts:
    // `test_project`'s root has 3 requirements/tests/results of its own
    // (design/external/integration, contract/plain/smoke,
    // design/external/integration) plus one more of each nested under
    // `alpha` (spec/alpha_test/spec) = 4 each; `Submodules: 3` counts
    // alpha, beta, and alpha's own child alpha_child.
    wait_until(&mut harness, |h| {
        h.query_by_label("Requirements: 4").is_some()
    });
    assert!(harness.query_by_label("Submodules: 3").is_some());
    assert!(harness.query_by_label("Test Procedures: 4").is_some());
    assert!(harness.query_by_label("Results: 4").is_some());
    assert!(
        harness
            .query_by_label("Project not validated — met/pass/fail statistics unavailable.")
            .is_some()
    );
}

/// Also covers the root page's pass/fail stats updating live once
/// Validate completes, per the same button — see `test_project`'s own
/// "design" requirement (`the_requirement_viewer_explains_why_it_is_unmet`)
/// for why not every requirement in it validates clean.
#[test]
fn validating_updates_the_root_pages_stats_in_place() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);
    wait_until(&mut harness, |h| {
        h.query_by_label("Requirements: 4").is_some()
    });
    assert!(
        harness
            .query_by_label("Project not validated — met/pass/fail statistics unavailable.")
            .is_some()
    );

    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_label("Project not validated — met/pass/fail statistics unavailable.")
            .is_none()
    });
    // The results breakdown only ever renders once `summary.validated` is
    // true — its presence alone proves this is a real refresh of the
    // still-open root page (not, say, a coincidentally similar new one).
    // Of the 4 requirements: "design" is genuinely Unmet (stale test
    // reference, see `the_requirement_viewer_explains_why_it_is_unmet`),
    // "external" is a real Pass, "integration" a real Fail, and alpha's
    // "spec" also lands Unmet — 1 pass, 1 fail, 2 incomplete.
    assert!(harness.query_by_label("Pass: 1 (25%)").is_some());
    assert!(harness.query_by_label("Fail: 1 (25%)").is_some());
    assert!(harness.query_by_label("Incomplete: 2 (50%)").is_some());
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
    open_test_project(&mut harness);

    // `test_project`'s root-level submodules (alpha/beta —
    // `ModuleDraft::modules` is a `BTreeMap`, so the tree renders them in
    // sorted order) are all "not current" right after load (the project
    // root is current by default), so `.last()` reliably means "beta"'s
    // own glyph button — see `icons::MODULE_NOT_CURRENT` (Phosphor's
    // `FOLDER_NOTCH`). No other `Role::Button` shares this glyph.
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .last()
        .expect("no module buttons found")
        .click();
    harness.step();

    // Status bar and the module page's own heading can both read "Module:
    // beta" simultaneously — `query_all_by_label` sidesteps
    // `query_by_label`'s panic-on-ambiguous-match behavior.
    assert!(harness.query_all_by_label("Module: beta").next().is_some());
    // `GetModuleSummary`'s reply goes through the real background actor —
    // wait for it rather than assuming it's already landed after one step.
    wait_until(&mut harness, |h| {
        h.query_by_label("Requirements: 0").is_some()
    });
    assert!(
        harness
            .query_by_label("Project not validated — met/pass/fail statistics unavailable.")
            .is_some()
    );

    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    // `Role::TextInput`, not bare `by_value` — a childless module's own
    // tree row is a plain `egui::Label` (see `render_tree_node`'s two
    // branches), and it turns out a `Label`'s accesskit *value* (not just
    // its label) also reports its text, so an unqualified `value("beta")`
    // query ambiguously matches that still-visible row instead of the
    // rename form's actual text field.
    wait_until(&mut harness, |h| {
        h.query_all(By::new().role(Role::TextInput).value("beta"))
            .next()
            .is_some()
    });

    let name_field = harness
        .get_all(By::new().role(Role::TextInput).value("beta"))
        .next()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("_renamed");
    harness.step();
    assert!(
        harness
            .query_all(By::new().role(Role::TextInput).value("beta_renamed"))
            .next()
            .is_some(),
        "name field did not become beta_renamed"
    );

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
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Edit").is_some()
    });
    assert!(harness.query_by_label("beta_renamed").is_some());
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn result_form_requirement_path_picker_fills_the_field() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Baseline count of bare "design"-labelled nodes before the form even
    // opens: zero — the "design" requirement's own tree leaf carries a
    // status-glyph prefix ("\u{e32c} design", not a bare match), and
    // results don't get their own tree row. Compared against the
    // post-pick count below to prove the picked value actually landed in
    // the form, since the field is a read-only label now (not a
    // `text_edit`), so `get_all_by_value` doesn't apply to it the way it
    // does to the still-editable `test_path` field's own picker test.
    let design_label_count_before = harness.query_all_by_label("design").count();

    harness
        .get_by_role_and_label(Role::Button, "New Result")
        .click();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Result")
            .is_some()
    );

    // Two "Pick…" buttons exist in this form (requirement path, then test
    // path) — the first in tree order is the requirement-path one. Opens
    // the shared path-picker modal (`render_path_picker_dialog`), which
    // replaced the old per-field `ComboBox` (see that function's own doc
    // comment on why: a `ComboBox` popup doesn't scroll, so it can't
    // handle a project with enough requirements to overflow the screen).
    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("requirement path picker not found")
        .click();
    harness.step();
    harness.step(); // let the modal settle — see this file's module doc.

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a requirement")
            .is_some()
    );

    // "design" is a real root-level requirement in `test_project`; the
    // modal row's text is `LogicalPath`'s own `Display` (bare "design"
    // for a root-level entry — no "modules/..." prefix, unlike the tree's
    // own leaf button, which additionally prefixes a status glyph —
    // "\u{e32c} design" — so the two don't collide). `.last()` is used
    // rather than `.next()`/`.first()` on principle, in case some other
    // "design"-labelled node is ever added alongside the modal row while
    // it's open — the modal renders last in `ui()`, so its row is always
    // the last match regardless.
    harness
        .get_all_by_label("design")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a requirement")
            .is_none()
    );
    // The picked requirement now shows up as one more "design"-labelled
    // node than the pre-pick baseline — this form's own read-only display
    // of `form.requirement`.
    assert_eq!(
        harness.get_all_by_label("design").count(),
        design_label_count_before + 1
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Result")
            .is_some()
    );
}

#[test]
fn result_form_test_path_picker_fills_the_field() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Result")
        .click();
    harness.step();

    // Same modal mechanics as the requirement-path picker above — two
    // "Pick…" buttons exist in this form; the test-path one is second in
    // tree order.
    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .nth(1)
        .expect("test path picker not found")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a test procedure")
            .is_some()
    );

    // "smoke" is a real root-level test in `test_project`. As
    // with "design" above, the tree's own leaf button for it also
    // matches by label and sorts first in tree order, so `.last()` is what
    // actually reaches the modal's own row.
    harness
        .get_all_by_label("smoke")
        .last()
        .expect("modal row not found")
        .click();
    harness.step();
    harness.step();

    assert!(harness.get_all_by_value("/tests/smoke").next().is_some());
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "New Result")
            .is_some()
    );
}

#[test]
fn path_picker_dialogs_filter_field_narrows_the_list() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Result")
        .click();
    harness.step();
    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("requirement path picker not found")
        .click();
    harness.step();
    harness.step();

    // `test_project` has (at least) two root-level requirements —
    // "design" and "external" — so both show unfiltered, each with one
    // match by label: the modal's own row. The tree's own leaf button
    // doesn't count (it carries a status-glyph prefix, not a bare match —
    // see the next test's own comment).
    assert_eq!(harness.get_all_by_label("design").count(), 1);
    assert_eq!(harness.get_all_by_label("external").count(), 1);

    // The modal's own filter field is its one `Role::TextInput` (the
    // dialog carries no other text field) — narrowing it to "ext" should
    // drop the modal's own "design" row entirely (no bare-labelled
    // "design" node remains, hence `query_all` rather than `get_all` to
    // avoid panicking on zero matches), while "external"'s modal row
    // still matches.
    let filter_field = harness
        .get_all_by_role(Role::TextInput)
        .last()
        .expect("filter field not found");
    filter_field.focus();
    filter_field.type_text("ext");
    harness.step();

    assert_eq!(harness.query_all_by_label("design").count(), 0);
    assert_eq!(harness.get_all_by_label("external").count(), 1);
}

#[test]
fn result_form_path_pickers_show_no_scope_radios() {
    // The scope radios (All/This module/Submodules) only make sense for
    // the requirement form's own Dependency/TestReference pickers, which
    // have a real "owning module" to scope relative to — a not-yet-saved
    // result has none, so its two pickers (requirement path, then test
    // path) keep the old unscoped behavior with no radios at all.
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_by_role_and_label(Role::Button, "New Result")
        .click();
    harness.step();

    harness
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("requirement path picker not found")
        .click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Pick a requirement")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::RadioButton, "This module")
            .is_none()
    );
}

#[test]
fn requirement_form_dependency_picker_scopes_by_this_module_and_submodules() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Select "alpha" as the current module so its own leaves ("spec",
    // "alpha_test") render in the bottom pane. With "Expand All" already
    // run by `open_test_project`, the top tree pane's "not current" glyph
    // buttons are, in order, alpha, alpha_child, beta (see
    // `switching_selected_module_updates_the_bottom_pane`'s own comment on
    // this exact ordering), so alpha's is the first.
    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .next()
        .expect("alpha's module button not found")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_all_by_label("Module: alpha").next().is_some()
    });

    // "spec" is alpha's own real requirement
    // (`test_project/modules/alpha/requirements/spec`) — open it for
    // editing so the picker's owning module is `["alpha"]`.
    open_leaf_for_editing(&mut harness, "\u{e32c} spec", "Edit Requirement");

    harness
        .get_by_role_and_label(Role::Button, "Add dependency")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Label, "Add Dependency")
        .parent()
        .expect("dependency composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("dependency composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step();

    // The radio queries below are scoped to the path-picker modal itself
    // (via its "Pick a requirement" heading, same technique as the
    // composer-scoped "Pick…" lookup above) rather than the whole
    // document — the dependency composer sitting underneath it (still
    // mounted, just covered) has its own unrelated `DependencyDraft` kind
    // picker with an "All submodules" radio of its own (see
    // `render_dependency_kind_picker`); the queries below stay scoped to
    // the path-picker modal to keep this test independent of that.

    // "All" (the default): every requirement in the project shows —
    // root-level ones and alpha's own nested "spec" alike. The modal row's
    // text is `LogicalPath`'s own `Display`, which prefixes nested entries
    // with their module path ("modules/alpha/spec"), unlike the bare
    // root-level "design".
    assert!(
        harness
            .get_by_role_and_label(Role::Label, "Pick a requirement")
            .parent()
            .expect("path picker modal container not found")
            .get_by_role_and_label(Role::RadioButton, "All")
            .accesskit_node()
            .toggled()
            == Some(Toggled::True)
    );
    assert_eq!(harness.get_all_by_label("design").count(), 1);
    assert_eq!(harness.get_all_by_label("modules/alpha/spec").count(), 1);

    // "This module": only alpha's own "spec" remains — the root-level
    // "design" disappears.
    harness
        .get_by_role_and_label(Role::Label, "Pick a requirement")
        .parent()
        .expect("path picker modal container not found")
        .get_by_role_and_label(Role::RadioButton, "This module")
        .click_accesskit();
    harness.step();

    assert_eq!(harness.query_all_by_label("design").count(), 0);
    assert_eq!(harness.get_all_by_label("modules/alpha/spec").count(), 1);

    // "Submodules": alpha's own "spec" now disappears too (it's *in*
    // alpha, not a submodule below it) — alpha's own child module
    // (`alpha_child`) has no requirements of its own, so nothing at all
    // matches.
    harness
        .get_by_role_and_label(Role::Label, "Pick a requirement")
        .parent()
        .expect("path picker modal container not found")
        .get_by_role_and_label(Role::RadioButton, "Submodules")
        .click_accesskit();
    harness.step();

    assert_eq!(harness.query_all_by_label("modules/alpha/spec").count(), 0);
    assert_eq!(harness.query_all_by_label("design").count(), 0);
    assert!(harness.query_by_label("No matches.").is_some());
}

#[test]
fn requirement_form_test_reference_picker_shows_scope_radios_and_scopes_by_this_module() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    harness
        .get_all_by_role_and_label(Role::Button, "\u{E24A}")
        .next()
        .expect("alpha's module button not found")
        .click();
    harness.step();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_all_by_label("Module: alpha").next().is_some()
    });

    open_leaf_for_editing(&mut harness, "\u{e32c} spec", "Edit Requirement");

    harness
        .get_by_role_and_label(Role::Button, "Add test procedure")
        .click_accesskit();
    harness.step();
    harness.step();

    harness
        .get_by_role_and_label(Role::Label, "Add Test Procedure")
        .parent()
        .expect("test reference composer modal container not found")
        .get_all_by_role_and_label(Role::Button, "Pick…")
        .next()
        .expect("test reference composer Pick button not found")
        .click_accesskit();
    harness.step();
    harness.step();

    // "smoke" is a real root-level test; "alpha_test" is alpha's own.
    // Both show under the default "All" scope.
    assert_eq!(harness.get_all_by_label("smoke").count(), 1);
    assert_eq!(
        harness.get_all_by_label("modules/alpha/alpha_test").count(),
        1
    );

    harness
        .get_by_role_and_label(Role::RadioButton, "This module")
        .click_accesskit();
    harness.step();

    // Only alpha's own "alpha_test" remains — the root-level "smoke"
    // disappears.
    assert_eq!(harness.query_all_by_label("smoke").count(), 0);
    assert_eq!(
        harness.get_all_by_label("modules/alpha/alpha_test").count(),
        1
    );
}

#[test]
fn editing_an_existing_result_can_add_a_local_attachment() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Results no longer get their own tree row — the only path to one is
    // via its owning requirement's viewer/form, which lists each result as
    // a `ui.link` reading "{title} ({status:?})" (see `view.rs`'s
    // `Results:` section) — though egui_kittest reports its accesskit
    // role as `Label`, not `Link`, hence the query role below. Open the
    // "design" requirement first, then follow its one result's link —
    // "Design" is the result's `title` field in the fixture, distinct
    // from the requirement's own name "design".
    harness
        .get_by_role_and_label(Role::Button, "\u{e32c} design")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Design (Incomplete)")
            .is_some()
    });
    harness
        .get_by_role_and_label(Role::Label, "Design (Incomplete)")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Edit").is_some()
    });
    // One more `step()` than usual before this click: the Result viewer
    // just landed off a nested navigation (requirement viewer -> result
    // viewer) two `wait_until`s deep, and clicking "Edit" immediately
    // (real pointer coordinates, not `click_accesskit`) misses — the
    // layout is still one frame behind where the button's rect settles.
    harness.step();
    // Re-confirm presence after that settle step rather than assuming it
    // held — a background `Event` landing during that one extra `step()`
    // (this test is two nested navigations and `wait_until`s deep, with a
    // real actor delivering completions on its own schedule) can
    // transiently redraw the pane, and the panicking getter right below
    // shouldn't run against a frame where the button just isn't there
    // yet. Under normal timing this returns immediately, since the
    // condition is already true.
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Button, "Edit").is_some()
    });
    harness.get_by_role_and_label(Role::Button, "Edit").click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_role_and_label(Role::Label, "Edit Result")
            .is_some()
    });

    // Field order among `Role::TextInput` nodes in edit mode: the status
    // bar's own zoom field(0) and the left pane's own filter field(1) —
    // both always first — then name(2), title(3), requirement_commit(4),
    // test_path(5), test_commit(6), then — since this form is editing an
    // existing entry — the local-attachment path field(7). No
    // `requirement_path` text field any more — the requirement is a
    // read-only label now (picked structurally, not typed), so it doesn't
    // count as a `TextInput`. Same "TextInput vs. MultilineTextInput" and
    // tree-order reasoning as the Requirement form's own attachment test.
    let attachment_path_field = harness
        .get_all_by_role(Role::TextInput)
        .nth(7)
        .expect("local-attachment path field not found");
    attachment_path_field.focus();
    attachment_path_field.type_text("interaction_test_result_attachment.md");
    harness.step();

    harness.get_by_role_and_label(Role::Button, "Add").click();
    harness.step();

    wait_until(&mut harness, |h| {
        h.query_by_label("interaction_test_result_attachment.md")
            .is_some()
    });

    assert!(
        harness
            .query_by_label("interaction_test_result_attachment.md")
            .is_some()
    );
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

#[test]
fn editing_an_existing_test_can_add_a_local_attachment_and_template_file() {
    let mut harness = harness();
    harness.step();
    open_test_project(&mut harness);

    // Same bare-name reasoning as the result leaf above — only
    // requirements carry a status glyph.
    open_leaf_for_editing(&mut harness, "smoke", "Edit Test Procedure");

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

    wait_until(&mut harness, |h| {
        h.query_by_label("interaction_test_test_attachment.md")
            .is_some()
    });
    assert!(
        harness
            .query_by_label("interaction_test_test_attachment.md")
            .is_some()
    );

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

    wait_until(&mut harness, |h| {
        h.query_by_label("interaction_test_template.md").is_some()
    });
    assert!(
        harness
            .query_by_label("interaction_test_template.md")
            .is_some()
    );
    assert!(harness.query_by_label("\u{e18a} unsaved changes").is_some());
}

/// Blocks every `write`/`create_dir_all`/`remove_dir_all` call on `gate`
/// until the test releases it — everything else delegates straight to
/// `inner`. Used by `exit_dialog_saving_then_timeout_lets_the_user_exit_anyway_or_keep_waiting`
/// in place of `syscalls::SlowFilesystem`'s fixed real-time delay: a fixed
/// delay is still a race (a real `Save` completing before the exit
/// dialog's own deadline is ever observed loses the race under enough CPU
/// contention — confirmed empirically, this test kept failing
/// intermittently under a full parallel `cargo test` run even after
/// widening `wait_until`'s own budget), where blocking on an explicit
/// release makes `Saving` stay true for as long as the test needs it to,
/// however long real time takes to get there.
#[derive(Clone)]
struct HangingWritesFilesystem<F> {
    inner: F,
    gate: SaveGate,
}

impl<F: syscalls::Filesystem> syscalls::Filesystem for HangingWritesFilesystem<F> {
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.inner.read_to_string(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.read_dir(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.inner.is_dir(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }

    fn write(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        self.gate.wait();
        self.inner.write(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.gate.wait();
        self.inner.create_dir_all(path)
    }

    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.gate.wait();
        self.inner.remove_dir_all(path)
    }
}

/// A one-shot, multi-waiter gate: any number of `wait()` calls block until
/// `release()` is called once, after which every past-or-future `wait()`
/// returns immediately. Plain `std::sync::{Mutex, Condvar}`, not a tokio
/// primitive — `wait()` runs from inside `spawn_blocking`, off the async
/// runtime entirely (same reasoning as `gui-core::actor::test::Gate`,
/// which this mirrors but can't share — that one's private to `gui-core`'s
/// own test module).
#[derive(Clone)]
struct SaveGate {
    inner: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl SaveGate {
    fn new() -> Self {
        SaveGate {
            inner: std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        }
    }

    fn wait(&self) {
        let (lock, condvar) = &*self.inner;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = condvar.wait(released).unwrap();
        }
    }

    fn release(&self) {
        let (lock, condvar) = &*self.inner;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }
}

#[test]
fn exit_dialog_saving_then_timeout_lets_the_user_exit_anyway_or_keep_waiting() {
    // This is also the one test in this file that completes a real
    // `Save`, so — unlike every other test here — it must run against
    // `scratch_copy_of_test_project`'s writable copy, not the real
    // fixture: an earlier version of this test pointed its filesystem
    // wrapper at the real `test_project` directly, and a real `Save`
    // reaching disk permanently wrote a new module and reformatted every
    // `.ron` file into the repository's own working tree.
    let project_dir = scratch_copy_of_test_project("exit-dialog-saving");

    let save_gate = SaveGate::new();
    let core = gui_core::CoreHandle::start_with(
        HangingWritesFilesystem {
            inner: syscalls::StdFilesystem,
            gate: save_gate.clone(),
        },
        FixedGit,
    )
    .expect("test tokio runtime");
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

    harness
        .get_by_role_and_label(Role::Button, "New Module")
        .click();
    harness.step();
    // `.last()`, not `.get_by_role` (which requires uniqueness): the
    // status bar's own zoom field (see `zoom_field_value`) is always
    // present too, and — status bar renders before this dialog/form in
    // `ui()` — always comes first in tree order, so the dialog/form's
    // own field is reliably the *last* `Role::TextInput` match.
    let name_field = harness
        .get_all_by_role(Role::TextInput)
        .last()
        .expect("name field not found");
    name_field.focus();
    name_field.type_text("interaction_test_module");
    harness.step();
    harness
        .get_by_role_and_label(Role::Button, "Create")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label("\u{e18a} unsaved changes").is_some()
    });

    // `Save` only actually touches the filesystem (and so only actually
    // hits `save_gate`) against a `Validated` project — against a `Draft`
    // it fails immediately with `SaveError::NotValidated`, with no real
    // I/O at all, resolving `Saving` -> `Ready` almost instantly
    // regardless of the gate. `test_project` plus an empty new module
    // validates cleanly.
    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    wait_until(&mut harness, |h| {
        h.query_by_label_contains("pending").is_none()
    });

    harness.get_by_role_and_label(Role::Button, "File").click();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Exit").click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label("You have unsaved changes. Save before exiting?")
            .is_some()
    );

    // The dialog's own "Save" is the second in tree order — the
    // toolbar's persistent one, rendered earlier in the frame, is first
    // (same ambiguity as every other exit-dialog test's comment on this).
    harness
        .get_all_by_role_and_label(Role::Button, "Save")
        .last()
        .expect("dialog Save button not found")
        .click();
    harness.step();

    // The real `Save` is now permanently blocked on `save_gate`'s first
    // `write`/`create_dir_all` call — not raced against a fixed delay —
    // so `TimedOut` is guaranteed to eventually become observable
    // (however long real time takes to get there under whatever CPU
    // contention is happening) rather than possibly never appearing
    // because the save finished first.
    wait_until(&mut harness, |h| {
        h.query_by_label("Still saving — exit anyway and lose unsaved changes, or keep waiting?")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Keep waiting")
        .click();
    harness.step();
    // Re-arms `Saving` with a fresh 1ms deadline — the real save is
    // still blocked on `save_gate`, so this reaches `TimedOut` again
    // rather than resolving straight to `Ready`. Proves "Keep waiting"
    // genuinely re-arms `Saving` rather than just leaving the same
    // `TimedOut` state on screen.
    wait_until(&mut harness, |h| {
        h.query_by_label("Still saving — exit anyway and lose unsaved changes, or keep waiting?")
            .is_some()
    });

    harness
        .get_by_role_and_label(Role::Button, "Exit anyway")
        .click();
    // Normally resolves in two steps — one processes the click (setting
    // `Ready`), the next runs `take_ready_to_exit` (which consumes it,
    // sends `Command::Shutdown`, and closes the viewport) at the top of
    // `ui()` before rendering, same "effect needs a second step" pattern
    // as Discard (see `discard_on_the_exit_dialog_closes_it_and_proceeds`).
    // `wait_until`, not a hardcoded step count, though: under the CPU
    // contention of a full parallel `cargo test` run a settle has been
    // observed taking longer than two `step()`s, and all this needs to
    // confirm is the eventual state, not how many frames it took.
    wait_until(&mut harness, |h| {
        h.query_by_label("Still saving — exit anyway and lose unsaved changes, or keep waiting?")
            .is_none()
    });

    // "Exit anyway" only resolves the *dialog* — the real background
    // `Save` is still blocked on `save_gate` when that happens, and keeps
    // running independently of it. Release the gate so it can actually
    // finish before dropping `harness`: dropping (and so the
    // `CoreHandle`/`tokio::runtime::Runtime` it owns) blocks until that
    // in-flight `spawn_blocking` task completes, which would otherwise
    // hang forever waiting on a save this test never let proceed.
    save_gate.release();
    drop(harness);
    std::fs::remove_dir_all(&project_dir).ok();
}

#[cfg(all(feature = "debug-panel", debug_assertions))]
#[test]
fn debug_panel_opens_only_after_confirming_and_closes_without_reconfirming() {
    let mut harness = harness();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Debug")
            .is_none()
    );

    harness.get_by_role_and_label(Role::Button, "Debug").click();
    harness.step();
    harness.step(); // let the modal settle — see this file's module doc.
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Open the debug panel?")
            .is_some()
    );

    harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Open the debug panel?")
            .is_none()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Debug")
            .is_none()
    );

    harness.get_by_role_and_label(Role::Button, "Debug").click();
    harness.step();
    harness.step();
    harness.get_by_role_and_label(Role::Button, "Open").click();
    harness.step();
    harness.step();

    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Debug")
            .is_some()
    );
    // The button's own accessible name stays "Debug" in both states — only
    // its icon (`icons::DEBUG_PANEL_OPEN`/`DEBUG_PANEL_CLOSED`) reflects
    // open vs. closed, so this just re-finds the same button by role+label.
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Debug")
            .is_some()
    );

    // Clicking the same toggle again, now that the panel is open, closes
    // it directly — no confirmation needed a second time (only opening
    // it does), see `GuiApp::debug_panel_button_clicked`'s own comment.
    harness.get_by_role_and_label(Role::Button, "Debug").click();
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "Debug")
            .is_none()
    );
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
    harness
        .get_by_role_and_label(Role::Button, "Validate")
        .click();
    harness.step();
    assert!(harness.get_all_by_role(Role::Label).any(|node| {
        node.accesskit_node()
            .value()
            .is_some_and(|value| value.contains("Validate"))
    }));

    harness
        .get_by_role_and_label(Role::Button, "Tx Stall")
        .click();
    harness.step();
    assert!(
        harness
            .query_by_label_contains("Tx is currently stalled")
            .is_some()
    );
}

