//! Rendering only — every decision (what a click *means*) lives in
//! `lib.rs`'s plain methods (`on_exit_clicked`, `select`, ...); this file
//! just draws widgets and calls them. See README's "Logic: keep it out of
//! `update()`" and "Layout".

use std::path::PathBuf;

use gui_core::{
    EntryKind, EntryName, EntryStatus, LogicalPath, ReferencePath, RequirementMetStatus,
    ResultKindV1, TestUnmetReason, TreeNode, TreeSnapshot, UnmetReason,
};

use crate::{
    AutoCommitKind, DependencyDraft, DependencySlot, EditorState, ExitDialogState, GuiApp,
    LocalPoolKind, PathPickerTarget, PendingNavigation, PendingProjectAction, TestRefDraft,
    TestRefSlot, ThemeChoice, ValidateBeforeSaveDialogState, absolute_reference_path,
    flatten_leaf_paths, icons, leaf_kind_segment, theme_colors,
};

/// Pops a native OS folder picker (`rfd`) titled `title` — blocking, but
/// bounded by the user's own interaction with it, not by anything
/// gui-core does; see README's "Never block the render thread" for why
/// that's a deliberate, documented exception rather than a violation of
/// it. Shared by Open Project's own not-dirty click and its
/// confirmed-after-unsaved-changes resume path (`render_unsaved_changes_dialog`).
fn pick_project_folder(title: &str) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).pick_folder()
}

/// An icon-only action button for the toolbar/menu bar — `icon` is what's
/// actually drawn, but `label` (shown as a hover tooltip) is also forced
/// in as the button's own accessible name via `widget_info`, overriding
/// what egui would otherwise derive from the icon glyph itself. That's
/// what lets these go icon-only without also rewriting every existing
/// `tests/interaction.rs` lookup that finds a toolbar/menu button by its
/// old exact text (e.g. `Role::Button, "New Requirement"`) — from the
/// accessibility tree's perspective, and so from these tests' perspective,
/// nothing about the button's identity changed, only how it's drawn.
fn icon_button(ui: &mut egui::Ui, enabled: bool, icon: &str, label: &str) -> egui::Response {
    let response = ui
        .add_enabled(enabled, egui::Button::new(icon))
        .on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response
}

/// The menu bar's own flavor of `icon_button` — a dropdown menu is read as
/// a list of text, where an icon-only item would be far less scannable
/// than the toolbar's icon-only buttons (which get room to spread out and
/// a hover tooltip to fall back on); this keeps `label` visible alongside
/// its icon instead of hiding it behind a tooltip. Still overrides the
/// accessible name back to the bare `label` (same reasoning as
/// `icon_button`) — egui would otherwise fold the icon glyph into the
/// concatenated accessible text too, which'd break exact-match lookups on
/// these items in `tests/interaction.rs` just the same as an icon-only
/// button would.
fn icon_text_button(ui: &mut egui::Ui, enabled: bool, icon: &str, label: &str) -> egui::Response {
    let response = ui.add_enabled(enabled, egui::Button::new((icon, label)));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response
}

/// A requirement's own Status row — the icon/color-chip pair (reusing
/// `icons::status_icon`/`theme_colors::status_colors`, same as the tree's
/// own status glyph — see `render_leaf` — for visual consistency between
/// the two) plus a plain-text label, and, if `Unmet`, a bulleted line per
/// reason it isn't. Free function, not a method: called from inside
/// `render_requirement_form`'s own `&mut self.editor` borrow, where
/// calling back out to a `&self`/`&mut self` method isn't available (same
/// reason `render_dependency_fields` etc. are free functions too).
fn render_requirement_status(ui: &mut egui::Ui, status: &RequirementMetStatus) {
    let (entry_status, label) = match status {
        RequirementMetStatus::Unvalidated => (EntryStatus::Unvalidated, "Unvalidated"),
        RequirementMetStatus::Met => (EntryStatus::Met, "Met"),
        RequirementMetStatus::Unmet(_) => (EntryStatus::Unmet, "Unmet"),
    };
    let (fg, bg) = theme_colors::status_colors(ui.visuals().dark_mode, entry_status);
    ui.horizontal(|ui| {
        ui.label("Status:");
        ui.label(
            egui::RichText::new(icons::status_icon(entry_status))
                .color(fg)
                .background_color(bg),
        );
        ui.label(label);
    });
    if let RequirementMetStatus::Unmet(reason) = status {
        for line in describe_unmet_reason(reason) {
            ui.label(format!("• {line}"));
        }
    }
}

/// Human-readable lines explaining an `UnmetReason` — one line for
/// `UnknownRequirement`/`NoTests`/`NotYetSaved`, one per unsatisfied test
/// for `UnsatisfiedTests`. Display-formatting for `logical`/`gui-core`
/// data lives here in `gui-ui`, not those crates — same convention
/// `DependencyDraft`'s own `Display` impl (`forms.rs`) already follows.
fn describe_unmet_reason(reason: &UnmetReason) -> Vec<String> {
    match reason {
        UnmetReason::UnknownRequirement => vec!["This requirement could not be found.".to_string()],
        UnmetReason::NoTests => vec!["It has no tests.".to_string()],
        UnmetReason::NotYetSaved => {
            vec!["It hasn't been saved yet, so there's no known commit to check.".to_string()]
        }
        UnmetReason::UnsatisfiedTests(tests) => tests
            .iter()
            .map(|unsatisfied| {
                let why = match unsatisfied.reason {
                    TestUnmetReason::UnresolvedReference => {
                        "its reference doesn't resolve to a real test"
                    }
                    TestUnmetReason::TestNotYetSaved => "the test hasn't been saved yet",
                    TestUnmetReason::StaleReference => {
                        "its reference is stale (pointing at an old commit of the test)"
                    }
                    TestUnmetReason::NoPassingResult => "no current, passing result exists for it",
                };
                format!("Test \"{}\": {why}.", unsatisfied.test)
            })
            .collect(),
    }
}

/// Whether the requirement viewer's "Update Stale References" button
/// should show at all — `true` only when `status` actually names at least
/// one `TestUnmetReason::StaleReference`. Every other `UnmetReason` (no
/// tests, never saved, an unresolved reference, no passing result) isn't
/// something this button can fix — there's no "current commit" to point a
/// missing/unresolved reference at, and a merely-unsatisfied-by-results
/// reference isn't stale at all.
fn has_stale_test_reference(status: &RequirementMetStatus) -> bool {
    matches!(
        status,
        RequirementMetStatus::Unmet(UnmetReason::UnsatisfiedTests(tests))
            if tests.iter().any(|t| t.reason == TestUnmetReason::StaleReference)
    )
}

/// A stable-ish string identifying which entry `resizable_multiline` is
/// being rendered for — `None` (create mode) all share `"new"`, since
/// there's only ever one create-mode form open at a time.
fn entry_id_salt(editing_target: &Option<LogicalPath>) -> String {
    match editing_target {
        Some(path) => path.to_string(),
        None => "new".to_string(),
    }
}

/// Row height `resizable_multiline` sizes against — matches the previous
/// static 80px/40px (4-line/2-line) defaults it replaced.
const MULTILINE_ROW_HEIGHT: f32 = 20.0;
/// Never smaller than this many rows, even for empty text — big enough to
/// still read as "a text box" rather than a single-line field, small
/// enough not to waste space on a field nobody's filled in yet.
const MULTILINE_MIN_ROWS: usize = 2;
/// The largest *default* size content growth alone will reach — beyond
/// this the box stops growing on its own and the user drags it bigger by
/// hand, same as it always could.
const MULTILINE_MAX_DEFAULT_ROWS: usize = 4;

/// A multiline text box the user can drag taller or shorter, for the
/// requirement text/guidance fields — these routinely run longer than the
/// default handful of rows `text_edit_multiline` allows before scrolling.
/// Only resizes vertically; width already tracks the surrounding panel via
/// `desired_width(f32::INFINITY)`.
///
/// Defaults to a height that tracks `text`'s current line count, clamped
/// to `MULTILINE_MIN_ROWS..=MULTILINE_MAX_DEFAULT_ROWS` rows — empty or
/// short text starts small, text with four or more lines starts at the
/// four-line cap, and the user can still drag past that cap or back down
/// to the two-line floor. `egui::Resize` only consults `default_height`
/// the first time it sees a given id, and remembers whatever size the box
/// ends up at (manually resized or not) for every id it's seen before —
/// so `id_salt` must be unique per distinct field *and* per distinct
/// entry being edited (callers fold the entry's identity in), or this
/// content-tracking default would only ever apply to the very first entry
/// ever opened in a given box.
fn resizable_multiline(ui: &mut egui::Ui, id_salt: &str, text: &mut String) -> egui::Response {
    resizable_multiline_with_max_height(ui, id_salt, text, f32::INFINITY)
}

/// Same as `resizable_multiline`, but caps how tall the user can drag the
/// box — for callers (like the commit-all dialog) that aren't inside their
/// own scroll area and would otherwise let a drag push the surrounding
/// modal/window past the screen's edge.
fn resizable_multiline_with_max_height(
    ui: &mut egui::Ui,
    id_salt: &str,
    text: &mut String,
    max_height: f32,
) -> egui::Response {
    let default_rows = text
        .lines()
        .count()
        .max(1)
        .clamp(MULTILINE_MIN_ROWS, MULTILINE_MAX_DEFAULT_ROWS);
    let available_width = ui.available_width();
    egui::Resize::default()
        .id_salt(id_salt)
        .resizable([false, true])
        .default_width(available_width)
        .min_width(available_width)
        .max_width(available_width)
        .default_height(MULTILINE_ROW_HEIGHT * default_rows as f32)
        .min_height(MULTILINE_ROW_HEIGHT * MULTILINE_MIN_ROWS as f32)
        .max_height(max_height)
        .show(ui, |ui| {
            // `TextEdit` otherwise only grows to fit its text (or
            // `desired_rows`' default of 4 lines) rather than the space
            // `Resize` just gave it, so the drag handle would visibly move
            // the frame without the box inside it following — `add_sized`
            // with the ui's own available size is what makes it fill the
            // frame instead.
            ui.add_sized(ui.available_size(), egui::TextEdit::multiline(text))
        })
}

/// `count / total` as a percentage, `0.0` for an empty `total` rather than
/// dividing by zero — the module/project page's Pass/Fail/Incomplete and
/// "Requirements met" lines all go through this.
fn percentage(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (count as f64 / total as f64) * 100.0
    }
}

impl GuiApp {
    /// Shared by the toolbar's and the File menu's "Save" — falls back
    /// to the same native folder picker "Save As…" uses when the current
    /// project doesn't have a known path yet (a `NewProject` never saved
    /// before), rather than sending a `Command::Save` gui-core can only
    /// answer with `Outcome::NoProjectLoaded`. See
    /// `GuiApp::needs_path_before_saving`'s own doc comment.
    fn save_button_clicked(&mut self) {
        if self.needs_path_before_saving() {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Save Project As")
                .pick_folder()
            {
                self.save_project_as(path);
            }
        } else {
            self.save_clicked();
        }
    }

    pub(crate) fn render_menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    // Both New Project and Open Project would otherwise
                    // discard `self.dirty` content wholesale — gated on
                    // it the same way (a plain field read, same as the
                    // status bar's own dirty check), routing through the
                    // unsaved-changes prompt instead of proceeding
                    // directly when there's something to lose. See
                    // `PendingProjectAction`'s own doc comment.
                    if icon_text_button(ui, true, icons::NEW_PROJECT, "New Project…").clicked() {
                        if self.dirty {
                            self.unsaved_changes_dialog_opened(PendingProjectAction::NewProject);
                        } else {
                            self.new_project_dialog_opened();
                        }
                        ui.close();
                    }
                    if icon_text_button(ui, true, icons::OPEN_PROJECT, "Open Project…").clicked()
                    {
                        if self.dirty {
                            self.unsaved_changes_dialog_opened(PendingProjectAction::OpenProject);
                        } else if let Some(path) = pick_project_folder("Open Project") {
                            self.open_project(path);
                        }
                        ui.close();
                    }
                    // Only rendered with something in it — an empty,
                    // permanently-disabled submenu would just be clutter.
                    if !self.recent.paths.is_empty() {
                        ui.menu_button("Open Recent", |ui| {
                            // Cloned up front: the loop body calls `self`
                            // methods that need `&mut self`, which can't
                            // coexist with an active borrow of
                            // `self.recent.paths` itself.
                            for path in self.recent.paths.clone() {
                                if ui.button(path.display().to_string()).clicked() {
                                    if self.dirty {
                                        self.unsaved_changes_dialog_opened(
                                            PendingProjectAction::OpenRecent(path),
                                        );
                                    } else {
                                        self.open_project(path);
                                    }
                                    ui.close();
                                }
                            }
                        });
                    }
                    let has_project = self.tree.is_some();
                    // Disabled with nothing loaded — a click would only
                    // ever come back `Outcome::NoProjectLoaded`, which
                    // gui-ui doesn't surface anywhere; better to not
                    // offer the click at all than silently swallow it.
                    if icon_text_button(ui, has_project, icons::SAVE, "Save").clicked() {
                        self.save_button_clicked();
                        ui.close();
                    }
                    if icon_text_button(ui, has_project, icons::SAVE_AS, "Save As…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_title("Save Project As")
                            .pick_folder()
                        {
                            self.save_project_as(path);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if icon_text_button(ui, true, icons::EXIT, "Exit").clicked() {
                        self.on_exit_clicked();
                        ui.close();
                    }
                });
                // Edit/View: deliberately not added yet — there's nothing
                // for either to meaningfully do until the center pane has
                // real per-kind forms (Edit) or view options worth toggling
                // (View). A menu with items that do nothing would be worse
                // than no menu.

                // The debug panel's own toggle, pinned to the menu bar's
                // far right corner — same `right_to_left` sub-layout
                // technique the status bar's zoom controls use. Entirely
                // absent from a non-`debug-panel` build, not just
                // disabled — see that Cargo feature's own doc comment on
                // why.
                #[cfg(all(feature = "debug-panel", debug_assertions))]
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let icon = if self.debug.open {
                        icons::DEBUG_PANEL_OPEN
                    } else {
                        icons::DEBUG_PANEL_CLOSED
                    };
                    if icon_text_button(ui, true, icon, "Debug").clicked() {
                        self.debug_panel_button_clicked();
                    }
                });
            });
        });
    }

    pub(crate) fn render_toolbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            // `horizontal_wrapped`, not `horizontal` — with Undo/Redo/
            // Back/Forward added alongside the original buttons, a plain
            // `horizontal` overflows a merely-800px-wide window (egui's
            // own default, and not an unreasonable width for a real
            // one): the row just keeps extending past the visible edge
            // rather than wrapping, silently pushing "Attachments…" half
            // off-screen — no visible clipping warning, just a button
            // that quietly stops being clickable past whatever width the
            // window happens to be. Found by a real interaction test
            // failing after these buttons were added, not by inspection.
            ui.horizontal_wrapped(|ui| {
                // Disabled with nothing loaded — same reasoning as the
                // File menu's own Save item, see `render_menu_bar`. Icon-
                // only (unlike the File menu's own items) — see
                // `icon_button`'s own doc comment on why the toolbar and
                // menu bar each get a different flavor of icon button.
                if icon_button(ui, self.tree.is_some(), icons::SAVE, "Save").clicked() {
                    self.save_button_clicked();
                }
                if icon_button(
                    ui,
                    self.tree.is_some(),
                    icons::COMMIT_ALL,
                    "Commit all changes…",
                )
                .clicked()
                {
                    self.commit_all_button_clicked();
                }
                if icon_button(ui, true, icons::VALIDATE, "Validate").clicked() {
                    self.validate_clicked();
                }
                ui.separator();
                // `can_undo`/`can_redo` come from `self.tree` (`gui-core`'s
                // own bookkeeping, piggybacked on `TreeSnapshot` — see
                // that type's own doc comment), not tracked locally —
                // disabled with nothing loaded at all, same as every
                // button here that needs a real project underneath it.
                let can_undo = self.tree.as_ref().is_some_and(|tree| tree.can_undo);
                if icon_button(ui, can_undo, icons::UNDO, "Undo").clicked() {
                    self.undo_clicked();
                }
                let can_redo = self.tree.as_ref().is_some_and(|tree| tree.can_redo);
                if icon_button(ui, can_redo, icons::REDO, "Redo").clicked() {
                    self.redo_clicked();
                }
                ui.separator();
                // `can_go_back`/`can_go_forward` are `gui-ui`'s own local
                // `nav_history` bookkeeping — unlike Undo/Redo, `gui-core`
                // has no reason to know about this at all (see
                // `nav_history`'s own doc comment).
                // Every one of these — Back/Forward and the four "New
                // ___" buttons — silently replaces or clears `self.editor`
                // exactly the way a tree click does, so they're gated on
                // unsaved form edits the same way (see
                // `PendingNavigation`'s own doc comment; Exit is
                // deliberately excluded, it has its own separate
                // `self.dirty`-driven prompt, see "Exit").
                if icon_button(ui, self.can_go_back(), icons::BACK, "Back").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::Back);
                    } else {
                        self.back_clicked();
                    }
                }
                if icon_button(ui, self.can_go_forward(), icons::FORWARD, "Forward").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::Forward);
                    } else {
                        self.forward_clicked();
                    }
                }
                ui.separator();
                if icon_button(ui, true, icons::NEW_REQUIREMENT, "New Requirement").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewRequirement);
                    } else {
                        self.new_requirement_clicked();
                    }
                }
                if icon_button(ui, true, icons::NEW_TEST, "New Test").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewTest);
                    } else {
                        self.new_test_clicked();
                    }
                }
                if icon_button(ui, true, icons::NEW_RESULT, "New Result").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewResult);
                    } else {
                        self.new_result_clicked();
                    }
                }
                if icon_button(ui, true, icons::NEW_MODULE, "New Module").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewModule);
                    } else {
                        self.new_module_clicked();
                    }
                }
                ui.separator();
                if icon_button(ui, true, icons::ATTACHMENTS, "Attachments…").clicked() {
                    self.attachments_dialog_opened();
                }
            });
        });
    }

    pub(crate) fn render_status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.tree.is_none() {
                    ui.label("No project loaded");
                } else if self.dirty {
                    ui.label(format!("{} unsaved changes", icons::UNSAVED));
                } else {
                    ui.label("saved");
                }
                if !self.pending.is_empty() {
                    ui.separator();
                    ui.label(format!("{} pending…", self.pending.len()));
                }
                ui.separator();
                let module_label = if self.selected_module.is_empty() {
                    "(project root)".to_string()
                } else {
                    self.selected_module
                        .iter()
                        .map(EntryName::as_str)
                        .collect::<Vec<_>>()
                        .join("/")
                };
                ui.label(format!("Module: {module_label}"));
                // TODO: project path, last validation outcome, once
                // Event::ValidationFailed is surfaced into self.status.

                // Zoom controls (plus the theme selector, to their left),
                // pinned to the status bar's far right — added in reverse
                // (`+` first) since `right_to_left` places each new widget
                // further left of the last, starting from the right edge;
                // this order reads "[theme] Reset − [value]% +" left-to-
                // right, `+`/`−` bracketing the editable value per the
                // usual zoom-control convention, Reset furthest left of
                // the zoom group since it's the least frequently used of
                // the four, and the theme selector coded last so it lands
                // furthest left of all — one more click to reach than
                // zoom, matching how rarely it's touched.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("+").clicked() {
                        self.zoom_in_clicked();
                    }
                    ui.label("%");
                    // Free-form text, not a numeric-only widget — any
                    // text is accepted while typing, only validated
                    // (parsed, clamped, and — either way — the field
                    // resynced to whatever the real applied value ends
                    // up being) once focus leaves it. See
                    // `GuiApp::zoom_input_submitted`.
                    let response = ui
                        .add(egui::TextEdit::singleline(&mut self.zoom_input).desired_width(30.0));
                    if response.lost_focus() {
                        self.zoom_input_submitted();
                    }
                    if ui.button("−").clicked() {
                        self.zoom_out_clicked();
                    }
                    if ui.button("Reset").clicked() {
                        self.zoom_reset_clicked();
                    }

                    let mut selected_theme = None;
                    egui::ComboBox::from_id_salt("theme_selector")
                        .selected_text(self.config.theme.label())
                        .show_ui(ui, |ui| {
                            for choice in ThemeChoice::ALL {
                                if ui
                                    .selectable_label(self.config.theme == choice, choice.label())
                                    .clicked()
                                {
                                    selected_theme = Some(choice);
                                }
                            }
                        });
                    if let Some(theme) = selected_theme {
                        self.theme_selected(theme);
                    }
                });
            });
        });
    }

    pub(crate) fn render_left_pane(&mut self, ui: &mut egui::Ui) {
        // Explicit range rather than egui's default 96.0..=infinity: wide
        // enough to comfortably fit a deeply-nested module/requirements-
        // tests-results tree without either pane getting crushed, still
        // bounded so dragging can't swallow the whole window.
        egui::Panel::left("tree_pane")
            .default_size(240.0)
            .size_range(120.0..=900.0)
            .show(ui, |ui| {
                // `ScrollArea` auto-shrinks to its content's width by default
                // (`auto_shrink`'s doc: "shrinks the scroll area to fit its
                // content"), which fights a resizable `Panel`: the panel
                // reports the width the user just dragged to, but the
                // auto-shrunk content immediately reports back a *narrower*
                // natural size next frame, so the drag can barely move it at
                // all — the panel keeps snapping back toward content width.
                // `auto_shrink([false, false])` makes it fill whatever width
                // the panel actually has instead.
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    // A bounded `desired_width`, not the default (which
                    // requests all remaining horizontal space in its row) —
                    // an unbounded singleline field here inflated this
                    // resizable panel's own measured natural width past its
                    // actual rendered width, which pushed the center pane's
                    // content (found via a real interaction test: the Result
                    // form's `ComboBox` trigger ended up positioned partway
                    // past the whole window's right edge, same "widget
                    // reports a rect beyond the visible viewport, so a click
                    // there lands nowhere" shape as the toolbar-overflow and
                    // high-zoom bugs already documented in this crate's
                    // README's Testing strategy).
                    ui.add(egui::TextEdit::singleline(&mut self.tree_filter).desired_width(150.0));
                    if ui.button("×").on_hover_text("Clear filter").clicked() {
                        self.tree_filter.clear();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Expand All").clicked() {
                        self.tree_force_open = Some(true);
                    }
                    if ui.button("Collapse All").clicked() {
                        self.tree_force_open = Some(false);
                    }
                });
                ui.separator();

                // Consumed here rather than left in `self` past this frame —
                // see `tree_force_open`'s own doc comment on why it's a
                // one-frame signal, not a persistent setting.
                let force_open = self.tree_force_open.take();

                // Cloned up front (rather than matching `&self.tree`) so the
                // borrow doesn't outlive this line — both closures below need
                // `&mut self` (for `select_module`/`render_selected_module_pane`),
                // which an immutable borrow of `self.tree` held across them
                // would conflict with.
                let tree = self.tree.clone();
                match &tree {
                    None => {
                        ui.label("No project loaded.");
                    }
                    Some(tree) => {
                        let root = tree.root.clone();

                        // Reserved *before* the top area is drawn: a
                        // `ScrollArea` with no `max_height` fills all
                        // available space in its container, so computing
                        // this split only after rendering the top area
                        // (as before) left nothing for the bottom area —
                        // its content was still drawn, just pushed below
                        // the visible panel.
                        let bottom_height = ui.available_height() * 0.4;
                        let top_height = (ui.available_height() - bottom_height - 8.0).max(0.0);

                        egui::ScrollArea::vertical()
                            .id_salt("tree_pane_modules")
                            .max_height(top_height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // The root `TreeNode`'s own `name` is the
                                // project's display name (see `gui-core`'s
                                // `build_tree_snapshot`) — a label, not a real
                                // module-path segment. Unlike every other
                                // `Module` node, it must never be pushed into a
                                // path, so the root is rendered specially here
                                // (its own selector + iterating its children
                                // with an *empty* path) rather than through
                                // `render_tree_node`, which pushes `node.name`
                                // for every module it handles.
                                let is_root_current = self.selected_module.is_empty();
                                ui.horizontal(|ui| {
                                    let glyph = if is_root_current {
                                        icons::MODULE_CURRENT
                                    } else {
                                        icons::MODULE_NOT_CURRENT
                                    };
                                    let mut text = egui::RichText::new(glyph);
                                    if is_root_current {
                                        text = text.color(theme_colors::module_current_color(
                                            ui.visuals().dark_mode,
                                        ));
                                    }
                                    if ui
                                        .add(egui::Button::new(text).small())
                                        .on_hover_text("Set as current module")
                                        .clicked()
                                    {
                                        self.select_module(Vec::new());
                                    }
                                    ui.strong(root.name.as_str());
                                });
                                render_module_children(self, ui, &root.children, &[], force_open);
                            });

                        ui.separator();

                        egui::ScrollArea::vertical()
                            .id_salt("tree_pane_selection")
                            .max_height(bottom_height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                render_selected_module_pane(self, ui, tree, force_open);
                            });
                    }
                }
            });
    }

    /// Dispatches on which of five things the center pane currently shows
    /// (nothing to show / loading / one of the four forms, each doing
    /// double duty as create-or-edit — see `forms.rs`). The `kind`
    /// pre-check exists only so the match arms below can call `&mut self`
    /// methods without still holding a borrow of `self.editor` — see each
    /// `render_*_form`'s own note.
    pub(crate) fn render_center_pane(&mut self, ui: &mut egui::Ui) {
        enum Pane {
            Empty,
            NewRequirement,
            NewTest,
            NewResult,
            NewModule,
            ExistingModule,
        }
        let pane = match &self.editor {
            EditorState::None => Pane::Empty,
            EditorState::NewRequirement(_) => Pane::NewRequirement,
            EditorState::NewTest(_) => Pane::NewTest,
            EditorState::NewResult(_) => Pane::NewResult,
            EditorState::NewModule(_) => Pane::NewModule,
            EditorState::ExistingModule(_) => Pane::ExistingModule,
        };

        egui::CentralPanel::default().show(ui, |ui| {
            // Each `render_*_form`/`render_module_page` manages its own
            // body `ScrollArea` now, so a form can run longer than the
            // window is tall (a requirement with several dependencies and
            // local attachments, say) without losing Save/Cancel off the
            // bottom edge (same overflow-past-viewport bug class as the
            // toolbar/zoom/filter-field fixes — see README's Testing
            // strategy) *and* keep its heading and buttons pinned above
            // that scrolling body. `Pane::Empty` has nothing worth
            // pinning a header above, so it's left unwrapped.
            match pane {
                // `self.editor` being `None` here means either nothing is
                // selected, or a selection's `GetEntryDetail` reply
                // hasn't landed yet — `select` clears `editor` up front
                // specifically so this branch can tell "nothing to show
                // yet" apart from "showing something."
                Pane::Empty => match &self.selection {
                    None => {
                        ui.label("Select an entry in the tree to view it, or use the toolbar to create a new one.");
                    }
                    Some(_) => {
                        ui.label("Loading…");
                    }
                },
                Pane::NewRequirement => self.render_requirement_form(ui),
                Pane::NewTest => self.render_test_form(ui),
                Pane::NewResult => self.render_result_form(ui),
                Pane::NewModule => self.render_module_form(ui),
                Pane::ExistingModule => self.render_module_page(ui),
            }
        });
    }

    /// Each `render_*_form` borrows `self.editor` mutably only inside its
    /// own block, capturing which button (if any) was clicked into a
    /// local — then, with that borrow dropped, calls the `&mut self`
    /// logic method the click means. Doing it inline (borrowing
    /// `self.editor` and calling `self.editor_create_clicked()` in the
    /// same scope) would conflict, since that method needs to reach
    /// `self.editor` itself.
    fn render_requirement_form(&mut self, ui: &mut egui::Ui) {
        let mut create_clicked = false;
        let mut cancel_clicked = false;
        let mut edit_clicked = false;
        let mut delete_clicked = false;
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        let mut auto_commit_clicked: Option<(DependencySlot, AutoCommitKind)> = None;
        let mut pick_dependency_path_clicked: Option<DependencySlot> = None;
        let mut test_ref_auto_commit_clicked: Option<(TestRefSlot, LogicalPath)> = None;
        let mut pick_test_ref_path_clicked: Option<TestRefSlot> = None;
        let mut refresh_stale_test_references_clicked = false;
        let mut recreate_clicked = false;
        // Set by a click on one of the read-only viewer's Dependencies/Test
        // references/Results links — acted on after `form`'s borrow of
        // `self.editor` ends below, same reasoning as every other
        // deferred-click flag in this function.
        let mut navigate_clicked: Option<(LogicalPath, EntryKind)> = None;
        {
            let EditorState::NewRequirement(form) = &mut self.editor else {
                return;
            };
            let editing = form.editing_target.is_some();
            let read_only = form.read_only;
            ui.horizontal(|ui| {
                ui.heading(if read_only {
                    "Requirement"
                } else if editing {
                    "Edit Requirement"
                } else {
                    "New Requirement"
                });
                // Only an already-existing entry's viewer has anything to
                // switch into editing — a create-mode form is already
                // editable, nothing to toggle.
                if read_only {
                    if ui.button("Edit").clicked() {
                        edit_clicked = true;
                    }
                    // Only when there's actually something for it to fix
                    // — see `has_stale_test_reference`'s own doc comment.
                    if has_stale_test_reference(&form.met_status) {
                        let busy = form.pending_request.is_some();
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new((
                                    icons::UPDATE_STALE_REFERENCES,
                                    "Update Stale References",
                                )),
                            )
                            .clicked()
                        {
                            refresh_stale_test_references_clicked = true;
                        }
                    }
                } else {
                    // Pinned next to the heading, not just at the
                    // bottom, so it's reachable without scrolling past
                    // however long the form runs (dependencies plus
                    // local attachments can push a bottom-only Save well
                    // past a modest window's fold) — same fix in spirit
                    // as the center pane's own `ScrollArea`, but for
                    // visibility rather than reachability.
                    let busy = form.pending_request.is_some();
                    let button_label = if editing { "Save" } else { "Create" };
                    if ui
                        .add_enabled(!busy, egui::Button::new(button_label))
                        .clicked()
                    {
                        create_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                    // Only an already-existing entry can be deleted — a
                    // create-mode form has nothing saved yet.
                    if editing && ui.add_enabled(!busy, egui::Button::new("Delete")).clicked() {
                        delete_clicked = true;
                    }
                    // Recreate is the only way to change a saved
                    // requirement's stable name — delete-then-recreate
                    // under a new name, rather than an in-place rename
                    // (there's no `RenameRequirement` command; see
                    // `RecreateRequirementState`'s own doc comment). Only
                    // offered for an already-existing entry, same as
                    // Delete.
                    if editing && ui.button("Recreate…").clicked() {
                        recreate_clicked = true;
                    }
                }
            });
        }
        // The heading and its buttons above render outside this
        // `ScrollArea` so the whole header row — not just the
        // Save/Cancel/Delete/Recreate buttons — stays reachable no
        // matter how long the form runs below.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let EditorState::NewRequirement(form) = &mut self.editor else {
                    return;
                };
                let editing = form.editing_target.is_some();
                let read_only = form.read_only;
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    if read_only {
                        ui.label(&form.name);
                    } else {
                        // Renaming isn't supported — an edit's target
                        // LogicalPath is fixed, so the name field is
                        // display-only once open in edit mode (see
                        // forms.rs's build_command).
                        if ui
                            .add_enabled(!editing, egui::TextEdit::singleline(&mut form.name))
                            .changed()
                        {
                            form.edited = true;
                        }
                    }
                });
                // Never editable, so it lives outside the read_only/editable
                // split below — but only meaningful once something's actually
                // been saved to check `met_status` against (a create-mode
                // form's `met_status` is always its own `Default`,
                // `Unvalidated`, which would just be noise to show here).
                if editing {
                    render_requirement_status(ui, &form.met_status);
                }
                if read_only {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        ui.label(&form.title);
                    });
                    ui.label("Requirement text:");
                    ui.label(&form.requirement_text);
                    ui.label("Requirement guidance:");
                    ui.label(&form.requirement_guidance);
                    ui.label("Test guidance:");
                    ui.label(&form.test_guidance);
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        if ui.text_edit_singleline(&mut form.title).changed() {
                            form.edited = true;
                        }
                    });
                    // Folded into every field's id_salt below so each
                    // distinct requirement gets its own remembered box size
                    // (and its own freshly content-tracked default) instead
                    // of sharing one across whichever requirement happened to
                    // be edited first — see `resizable_multiline`'s own doc
                    // comment.
                    let entry_id = entry_id_salt(&form.editing_target);
                    ui.label("Requirement text:");
                    if resizable_multiline(
                        ui,
                        &format!("requirement_text:{entry_id}"),
                        &mut form.requirement_text,
                    )
                    .changed()
                    {
                        form.edited = true;
                    }
                    ui.label("Requirement guidance:");
                    if resizable_multiline(
                        ui,
                        &format!("requirement_guidance:{entry_id}"),
                        &mut form.requirement_guidance,
                    )
                    .changed()
                    {
                        form.edited = true;
                    }
                    ui.label("Test guidance:");
                    if resizable_multiline(
                        ui,
                        &format!("test_guidance:{entry_id}"),
                        &mut form.test_guidance,
                    )
                    .changed()
                    {
                        form.edited = true;
                    }
                }
                if let Some(error) = &form.error {
                    ui.colored_label(egui::Color32::RED, error);
                }

                // Dependencies, unlike attachments below, aren't gated on
                // `editing` — a brand new requirement can have dependencies
                // set before it's ever created, since they're plain draft
                // data submitted whole on Save/Create, not a local file pool
                // requiring the entry to already exist.
                ui.separator();
                egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label("Dependencies:");
                let mut remove_dependency: Option<usize> = None;
                let mut dependency_edited = false;
                for (i, dep) in form.dependencies.iter_mut().enumerate() {
                    if read_only {
                        // Only `LocalRequirement` names another entry in
                        // this same project — `Remote` points outside it
                        // (nothing here to navigate to) and `Submodules`
                        // names no single entry at all, so both stay plain
                        // labels.
                        let target = match dep {
                            DependencyDraft::LocalRequirement { path, .. } => form
                                .editing_target
                                .as_ref()
                                .and_then(|t| {
                                    gui_core::resolve_reference_path(
                                        &ReferencePath(path.clone()),
                                        &t.modules,
                                        "requirements",
                                    )
                                }),
                            _ => None,
                        };
                        if let Some(target) = target {
                            if ui.link(dep.to_string()).clicked() {
                                navigate_clicked = Some((target, EntryKind::Requirement));
                            }
                        } else {
                            ui.label(dep.to_string());
                        }
                    } else {
                        ui.horizontal(|ui| {
                            dependency_edited |= render_dependency_kind_picker(ui, dep);
                            if ui.button("Remove").clicked() {
                                remove_dependency = Some(i);
                            }
                        });
                        let (changed, auto, pick_clicked) =
                            render_dependency_fields(ui, dep, self.tree.as_ref());
                        dependency_edited |= changed;
                        if let Some(kind) = auto {
                            auto_commit_clicked = Some((DependencySlot::Existing(i), kind));
                        }
                        if pick_clicked {
                            pick_dependency_path_clicked = Some(DependencySlot::Existing(i));
                        }
                    }
                }
                if let Some(i) = remove_dependency {
                    form.dependencies.remove(i);
                    dependency_edited = true;
                }
                if dependency_edited {
                    form.edited = true;
                }
                if let Some(error) = &form.commit_fetch_error {
                    ui.colored_label(egui::Color32::RED, error);
                }
                if !read_only {
                    ui.label("Add dependency:");
                    ui.horizontal(|ui| {
                        // Composing a not-yet-added entry isn't itself an
                        // edit to the form's real content — only actually
                        // clicking "Add dependency" below is, so this return
                        // value is deliberately ignored (unlike the existing-
                        // row loop above).
                        render_dependency_kind_picker(ui, &mut form.new_dependency);
                    });
                    let (_, auto, pick_clicked) =
                        render_dependency_fields(ui, &mut form.new_dependency, self.tree.as_ref());
                    if let Some(kind) = auto {
                        auto_commit_clicked = Some((DependencySlot::New, kind));
                    }
                    if pick_clicked {
                        pick_dependency_path_clicked = Some(DependencySlot::New);
                    }
                    if ui.button("Add dependency").clicked() {
                        form.dependencies.push(form.new_dependency.clone());
                        form.new_dependency = DependencyDraft::default();
                        form.edited = true;
                    }
                }
                });

                // Test references, like Dependencies above (and unlike Local
                // attachments below), aren't gated on `editing` — plain draft
                // data submitted whole on Save/Create, not a local file pool
                // requiring the entry to already exist.
                ui.separator();
                egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label("Test references:");
                let mut remove_test_ref: Option<usize> = None;
                let mut test_ref_edited = false;
                for (i, test_ref) in form.tests.iter_mut().enumerate() {
                    if read_only {
                        let target = form.editing_target.as_ref().and_then(|t| {
                            gui_core::resolve_reference_path(
                                &ReferencePath(test_ref.path.clone()),
                                &t.modules,
                                "tests",
                            )
                        });
                        if let Some(target) = target {
                            if ui.link(test_ref.to_string()).clicked() {
                                navigate_clicked = Some((target, EntryKind::Test));
                            }
                        } else {
                            ui.label(test_ref.to_string());
                        }
                    } else {
                        if ui.button("Remove").clicked() {
                            remove_test_ref = Some(i);
                        }
                        let (changed, auto, pick_clicked) =
                            render_test_ref_fields(ui, test_ref, self.tree.as_ref());
                        test_ref_edited |= changed;
                        if let Some(target) = auto {
                            test_ref_auto_commit_clicked = Some((TestRefSlot::Existing(i), target));
                        }
                        if pick_clicked {
                            pick_test_ref_path_clicked = Some(TestRefSlot::Existing(i));
                        }
                    }
                }
                if let Some(i) = remove_test_ref {
                    form.tests.remove(i);
                    test_ref_edited = true;
                }
                if test_ref_edited {
                    form.edited = true;
                }
                if let Some(error) = &form.test_commit_fetch_error {
                    ui.colored_label(egui::Color32::RED, error);
                }
                if !read_only {
                    ui.label("Add test reference:");
                    let (_, auto, pick_clicked) =
                        render_test_ref_fields(ui, &mut form.new_test_ref, self.tree.as_ref());
                    if let Some(target) = auto {
                        test_ref_auto_commit_clicked = Some((TestRefSlot::New, target));
                    }
                    if pick_clicked {
                        pick_test_ref_path_clicked = Some(TestRefSlot::New);
                    }
                    if ui.button("Add test reference").clicked() {
                        form.tests.push(form.new_test_ref.clone());
                        form.new_test_ref = TestRefDraft::default();
                        form.edited = true;
                    }
                }
                });

                // Results, unlike Dependencies/Test references above, are
                // read-only display data even in the editable form — a
                // result names its requirement, not the other way around
                // (see `Command::AddResult`), so there's nothing here to
                // add/remove/edit. Shown only for an already-existing
                // requirement, same as Local attachments below.
                if editing {
                    ui.separator();
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label("Results:");
                        if form.results.is_empty() {
                            ui.label("No results reference this requirement yet.");
                        }
                        for result in &form.results {
                            // Unlike Dependencies/Test references above,
                            // `result.path` is already a resolved
                            // `LogicalPath` (see `RequirementResult`), so
                            // there's no parsing step before it's clickable.
                            if ui
                                .link(format!(
                                    "{} ({:?}) — {}",
                                    result.title, result.status, result.path
                                ))
                                .clicked()
                            {
                                navigate_clicked = Some((result.path.clone(), EntryKind::Result));
                            }
                        }
                    });
                }

                // Local attachments only make sense for an already-existing
                // requirement — see `Command::AddRequirementAttachment`'s doc
                // comment (the entry has to exist first). The viewer shows
                // the list but not the Add/Remove controls — those mutate,
                // which the viewer doesn't do.
                if editing {
                    ui.separator();
                    ui.label("Local attachments:");
                    for path in &form.attachments {
                        if read_only {
                            ui.label(path.display().to_string());
                        } else {
                            ui.horizontal(|ui| {
                                ui.label(path.display().to_string());
                                if ui.button("Remove").clicked() {
                                    remove_attachment = Some(path.clone());
                                }
                            });
                        }
                    }
                    if !read_only {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.new_attachment_path);
                            if ui.button("Add").clicked() {
                                add_attachment_clicked = true;
                            }
                        });
                        if let Some(error) = &form.local_pool_error {
                            ui.colored_label(egui::Color32::RED, error);
                        }
                    }
                }
            });
        if edit_clicked {
            self.editor_edit_clicked();
        } else if create_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
        } else if delete_clicked {
            self.editor_delete_clicked();
        } else if recreate_clicked {
            self.recreate_requirement_clicked();
        } else if add_attachment_clicked {
            self.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);
        } else if let Some(path) = remove_attachment {
            self.local_attachment_remove_clicked(LocalPoolKind::RequirementAttachment, path);
        }
        if let Some((target, kind)) = auto_commit_clicked {
            self.dependency_commit_auto_clicked(target, kind);
        }
        if let Some(slot) = pick_dependency_path_clicked {
            self.path_picker_dialog_opened(PathPickerTarget::Dependency(slot));
        }
        if let Some((target, logical)) = test_ref_auto_commit_clicked {
            self.test_ref_commit_auto_clicked(target, logical);
        }
        if let Some(slot) = pick_test_ref_path_clicked {
            self.path_picker_dialog_opened(PathPickerTarget::TestReference(slot));
        }
        if refresh_stale_test_references_clicked {
            self.refresh_stale_test_references_clicked();
        }
        if let Some((target, kind)) = navigate_clicked {
            self.select(target, kind);
        }
    }

    fn render_test_form(&mut self, ui: &mut egui::Ui) {
        let mut create_clicked = false;
        let mut cancel_clicked = false;
        let mut edit_clicked = false;
        let mut delete_clicked = false;
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        let mut add_template_clicked = false;
        let mut remove_template: Option<PathBuf> = None;
        let mut recreate_clicked = false;
        {
            let EditorState::NewTest(form) = &mut self.editor else {
                return;
            };
            let editing = form.editing_target.is_some();
            let read_only = form.read_only;
            ui.horizontal(|ui| {
                ui.heading(if read_only {
                    "Test"
                } else if editing {
                    "Edit Test"
                } else {
                    "New Test"
                });
                if read_only {
                    if ui.button("Edit").clicked() {
                        edit_clicked = true;
                    }
                } else {
                    // See the Requirement form's own comment on why this
                    // lives next to the heading rather than only at the
                    // bottom.
                    let busy = form.pending_request.is_some();
                    let button_label = if editing { "Save" } else { "Create" };
                    if ui
                        .add_enabled(!busy, egui::Button::new(button_label))
                        .clicked()
                    {
                        create_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                    // Only an already-existing entry can be deleted — a
                    // create-mode form has nothing saved yet.
                    if editing && ui.add_enabled(!busy, egui::Button::new("Delete")).clicked() {
                        delete_clicked = true;
                    }
                    // See the Requirement form's own comment on Recreate —
                    // same reasoning, `RecreateTestState`.
                    if editing && ui.button("Recreate…").clicked() {
                        recreate_clicked = true;
                    }
                }
            });
        }
        // See the Requirement form's own comment on why the header
        // renders outside this `ScrollArea`.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let EditorState::NewTest(form) = &mut self.editor else {
                    return;
                };
                let editing = form.editing_target.is_some();
                let read_only = form.read_only;
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    if read_only {
                        ui.label(&form.name);
                    } else if ui
                        .add_enabled(!editing, egui::TextEdit::singleline(&mut form.name))
                        .changed()
                    {
                        form.edited = true;
                    }
                });
                if read_only {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        ui.label(&form.title);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Result kind:");
                        ui.label(match form.result_kind {
                            ResultKindV1::FreeForm => "Free Form",
                            ResultKindV1::Template => "Template",
                        });
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        if ui.text_edit_singleline(&mut form.title).changed() {
                            form.edited = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Result kind:");
                        if ui
                            .radio(
                                matches!(form.result_kind, ResultKindV1::FreeForm),
                                "Free Form",
                            )
                            .clicked()
                        {
                            form.result_kind = ResultKindV1::FreeForm;
                            form.edited = true;
                        }
                        if ui
                            .radio(
                                matches!(form.result_kind, ResultKindV1::Template),
                                "Template",
                            )
                            .clicked()
                        {
                            form.result_kind = ResultKindV1::Template;
                            form.edited = true;
                        }
                    });
                }
                if let Some(error) = &form.error {
                    ui.colored_label(egui::Color32::RED, error);
                }

                if editing {
                    ui.separator();
                    ui.label("Local attachments:");
                    for path in &form.attachments {
                        if read_only {
                            ui.label(path.display().to_string());
                        } else {
                            ui.horizontal(|ui| {
                                ui.label(path.display().to_string());
                                if ui.button("Remove").clicked() {
                                    remove_attachment = Some(path.clone());
                                }
                            });
                        }
                    }
                    if !read_only {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.new_attachment_path);
                            if ui.button("Add").clicked() {
                                add_attachment_clicked = true;
                            }
                        });
                    }

                    ui.separator();
                    ui.label("Local template files:");
                    for path in &form.template_files {
                        if read_only {
                            ui.label(path.display().to_string());
                        } else {
                            ui.horizontal(|ui| {
                                ui.label(path.display().to_string());
                                if ui.button("Remove").clicked() {
                                    remove_template = Some(path.clone());
                                }
                            });
                        }
                    }
                    if !read_only {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.new_template_path);
                            if ui.button("Add").clicked() {
                                add_template_clicked = true;
                            }
                        });

                        if let Some(error) = &form.local_pool_error {
                            ui.colored_label(egui::Color32::RED, error);
                        }
                    }
                }
            });
        if edit_clicked {
            self.editor_edit_clicked();
        } else if create_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
        } else if delete_clicked {
            self.editor_delete_clicked();
        } else if recreate_clicked {
            self.recreate_test_clicked();
        } else if add_attachment_clicked {
            self.local_attachment_add_clicked(LocalPoolKind::TestAttachment);
        } else if let Some(path) = remove_attachment {
            self.local_attachment_remove_clicked(LocalPoolKind::TestAttachment, path);
        } else if add_template_clicked {
            self.local_attachment_add_clicked(LocalPoolKind::TestTemplate);
        } else if let Some(path) = remove_template {
            self.local_attachment_remove_clicked(LocalPoolKind::TestTemplate, path);
        }
    }

    fn render_result_form(&mut self, ui: &mut egui::Ui) {
        let mut create_clicked = false;
        let mut cancel_clicked = false;
        let mut edit_clicked = false;
        let mut delete_clicked = false;
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        let mut open_picker: Option<PathPickerTarget> = None;
        {
            let EditorState::NewResult(form) = &mut self.editor else {
                return;
            };
            let editing = form.editing_target.is_some();
            let read_only = form.read_only;
            ui.horizontal(|ui| {
                ui.heading(if read_only {
                    "Result"
                } else if editing {
                    "Edit Result"
                } else {
                    "New Result"
                });
                if read_only {
                    if ui.button("Edit").clicked() {
                        edit_clicked = true;
                    }
                } else {
                    // See the Requirement form's own comment on why this
                    // lives next to the heading rather than only at the
                    // bottom.
                    let busy = form.pending_request.is_some();
                    let button_label = if editing { "Save" } else { "Create" };
                    if ui
                        .add_enabled(!busy, egui::Button::new(button_label))
                        .clicked()
                    {
                        create_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                    // Only an already-existing entry can be deleted — a
                    // create-mode form has nothing saved yet.
                    if editing && ui.add_enabled(!busy, egui::Button::new("Delete")).clicked() {
                        delete_clicked = true;
                    }
                }
            });
        }
        // See the Requirement form's own comment on why the header
        // renders outside this `ScrollArea`.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let EditorState::NewResult(form) = &mut self.editor else {
                    return;
                };
                let editing = form.editing_target.is_some();
                let read_only = form.read_only;
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    if read_only {
                        ui.label(&form.name);
                    } else if ui
                        .add_enabled(!editing, egui::TextEdit::singleline(&mut form.name))
                        .changed()
                    {
                        form.edited = true;
                    }
                });
                if read_only {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        ui.label(&form.title);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Requirement path:");
                        ui.label(&form.requirement_path);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Requirement commit:");
                        ui.label(&form.requirement_commit);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Test path:");
                        ui.label(&form.test_path);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Test commit:");
                        ui.label(&form.test_commit);
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        if ui.text_edit_singleline(&mut form.title).changed() {
                            form.edited = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Requirement path:");
                        if ui
                            .text_edit_singleline(&mut form.requirement_path)
                            .changed()
                        {
                            form.edited = true;
                        }
                        // A picker, not a replacement for the field above —
                        // typing the path by hand still works (e.g. pasting
                        // one, or for when the target hasn't loaded into
                        // `self.tree` yet). Opens the shared path-picker modal
                        // (`GuiApp::path_picker_dialog`) rather than an inline
                        // `ComboBox` — a project with enough requirements
                        // would otherwise overflow a `ComboBox` popup right
                        // off the screen, with no way to search it down to
                        // the one wanted. Selecting an entry there fills this
                        // same field with the correctly-formatted absolute
                        // reference path, so the user doesn't have to know
                        // `logical`'s `/[modules/<sub>/]*requirements/<name>`
                        // syntax by heart.
                        if self.tree.is_some() && ui.button("Pick…").clicked() {
                            open_picker = Some(PathPickerTarget::ResultRequirementPath);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Requirement commit:");
                        if ui
                            .text_edit_singleline(&mut form.requirement_commit)
                            .changed()
                        {
                            form.edited = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Test path:");
                        if ui.text_edit_singleline(&mut form.test_path).changed() {
                            form.edited = true;
                        }
                        if self.tree.is_some() && ui.button("Pick…").clicked() {
                            open_picker = Some(PathPickerTarget::ResultTestPath);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Test commit:");
                        if ui.text_edit_singleline(&mut form.test_commit).changed() {
                            form.edited = true;
                        }
                    });
                    ui.label(
                        "Commits aren't picked automatically yet — copy the target's \
                     current commit by hand (see README's Open questions).",
                    );
                }
                if let Some(error) = &form.error {
                    ui.colored_label(egui::Color32::RED, error);
                }

                if editing {
                    ui.separator();
                    ui.label("Local attachments:");
                    for path in &form.attachments {
                        if read_only {
                            ui.label(path.display().to_string());
                        } else {
                            ui.horizontal(|ui| {
                                ui.label(path.display().to_string());
                                if ui.button("Remove").clicked() {
                                    remove_attachment = Some(path.clone());
                                }
                            });
                        }
                    }
                    if !read_only {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.new_attachment_path);
                            if ui.button("Add").clicked() {
                                add_attachment_clicked = true;
                            }
                        });
                        if let Some(error) = &form.local_pool_error {
                            ui.colored_label(egui::Color32::RED, error);
                        }
                    }
                }
            });
        if edit_clicked {
            self.editor_edit_clicked();
        } else if create_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
        } else if delete_clicked {
            self.editor_delete_clicked();
        } else if add_attachment_clicked {
            self.local_attachment_add_clicked(LocalPoolKind::ResultAttachment);
        } else if let Some(path) = remove_attachment {
            self.local_attachment_remove_clicked(LocalPoolKind::ResultAttachment, path);
        }
        if let Some(target) = open_picker {
            self.path_picker_dialog_opened(target);
        }
    }

    fn render_module_form(&mut self, ui: &mut egui::Ui) {
        let mut create_clicked = false;
        let mut cancel_clicked = false;
        {
            let EditorState::NewModule(form) = &mut self.editor else {
                return;
            };
            ui.heading("New Module");
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut form.name);
            });
            if let Some(error) = &form.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            let creating = form.pending_request.is_some();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!creating, egui::Button::new("Create"))
                    .clicked()
                {
                    create_clicked = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
            });
        }
        if create_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
        }
    }

    /// The view/edit page for an already-existing module or the project
    /// root — see `ModuleDetailFormState`'s own doc comment. Same
    /// view/edit-in-one-function shape as `render_requirement_form` etc.,
    /// just without any local pools or dependencies to manage.
    fn render_module_page(&mut self, ui: &mut egui::Ui) {
        let mut edit_clicked = false;
        let mut save_clicked = false;
        let mut cancel_clicked = false;
        let mut delete_clicked = false;
        {
            let EditorState::ExistingModule(form) = &mut self.editor else {
                return;
            };
            let is_root = form.path.is_empty();
            ui.horizontal(|ui| {
                ui.heading(if is_root {
                    format!("Project: {}", form.display_name)
                } else {
                    format!("Module: {}", form.display_name)
                });
                if form.read_only {
                    if ui.button("Edit").clicked() {
                        edit_clicked = true;
                    }
                } else {
                    let busy = form.pending_request.is_some();
                    if ui.add_enabled(!busy, egui::Button::new("Save")).clicked() {
                        save_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                    // Never offered for the project root itself — see
                    // `render_module_page`'s exclusion in the user's
                    // original request ("except project edit").
                    if !is_root && ui.add_enabled(!busy, egui::Button::new("Delete")).clicked() {
                        delete_clicked = true;
                    }
                }
            });
        }
        // See the Requirement form's own comment on why the header
        // renders outside this `ScrollArea`.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let EditorState::ExistingModule(form) = &mut self.editor else {
                    return;
                };
                if form.read_only {
                    match &form.summary {
                        None => {
                            ui.label("Loading…");
                        }
                        Some(summary) => {
                            ui.label(format!("Submodules: {}", summary.submodule_count));
                            ui.label(format!("Requirements: {}", summary.requirement_count));
                            ui.label(format!("Tests: {}", summary.test_count));
                            ui.label(format!("Results: {}", summary.result_count));
                            ui.separator();
                            if summary.validated {
                                let met_pct =
                                    percentage(summary.requirements_met, summary.requirement_count);
                                ui.label(format!(
                                    "Requirements met: {} / {} ({met_pct:.0}%)",
                                    summary.requirements_met, summary.requirement_count
                                ));
                                let pass_pct =
                                    percentage(summary.results_pass, summary.result_count);
                                let fail_pct =
                                    percentage(summary.results_fail, summary.result_count);
                                let incomplete_pct =
                                    percentage(summary.results_incomplete, summary.result_count);
                                ui.label(format!(
                                    "Pass: {} ({pass_pct:.0}%)",
                                    summary.results_pass
                                ));
                                ui.label(format!(
                                    "Fail: {} ({fail_pct:.0}%)",
                                    summary.results_fail
                                ));
                                ui.label(format!(
                                    "Incomplete: {} ({incomplete_pct:.0}%)",
                                    summary.results_incomplete
                                ));
                            } else {
                                ui.label(
                                    "Project not validated — met/pass/fail statistics unavailable.",
                                );
                            }
                        }
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        if ui.text_edit_singleline(&mut form.new_name).changed() {
                            form.edited = true;
                        }
                    });
                    if let Some(error) = &form.error {
                        ui.colored_label(egui::Color32::RED, error);
                    }
                }
            });
        if edit_clicked {
            self.editor_edit_clicked();
        } else if save_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
        } else if delete_clicked {
            self.editor_delete_clicked();
        }
    }

    /// A project's *name*, unlike Open/Save As's target directory, isn't
    /// something a file picker can supply — so this stays a plain modal
    /// text field rather than following them to `rfd`. See
    /// `new_project_dialog`'s own doc comment.
    pub(crate) fn render_new_project_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(mut name) = self.new_project_dialog.clone() else {
            return;
        };

        let mut confirmed = false;
        let mut cancelled = false;
        egui::Modal::new(egui::Id::new("new_project_dialog")).show(ui.ctx(), |ui| {
            ui.heading("New Project");
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut name);
            });
            ui.horizontal(|ui| {
                if ui.button("Create").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            self.new_project_dialog = Some(name);
            self.new_project_dialog_confirmed();
        } else if cancelled {
            self.new_project_dialog_cancelled();
        } else {
            self.new_project_dialog = Some(name);
        }
    }

    /// The "you have unsaved changes" prompt — see `PendingProjectAction`'s
    /// own doc comment on what it's guarding. `unsaved_changes_confirmed`
    /// does everything Continue means *except* popping the native folder
    /// picker for `OpenProject`, which stays here (view-layer, `rfd`) —
    /// its return value tells this function whether that's needed.
    pub(crate) fn render_unsaved_changes_dialog(&mut self, ui: &mut egui::Ui) {
        if self.unsaved_changes_dialog.is_none() {
            return;
        }

        let mut confirmed = false;
        let mut cancelled = false;
        egui::Modal::new(egui::Id::new("unsaved_changes_dialog")).show(ui.ctx(), |ui| {
            ui.label("You have unsaved changes. Continue and lose them?");
            ui.horizontal(|ui| {
                if ui.button("Continue").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            if let Some(PendingProjectAction::OpenProject) = self.unsaved_changes_confirmed()
                && let Some(path) = pick_project_folder("Open Project")
            {
                self.open_project(path);
            }
        } else if cancelled {
            self.unsaved_changes_dialog_cancelled();
        }
    }

    /// The unsaved-*form*-edits prompt — see `PendingNavigation`'s own
    /// doc comment on what it guards and how it differs from
    /// `render_unsaved_changes_dialog` above. Deliberately distinct
    /// wording ("This form" vs. "You") so the two are never ambiguous
    /// even though only one can realistically be open at a time.
    pub(crate) fn render_unsaved_form_dialog(&mut self, ui: &mut egui::Ui) {
        if self.unsaved_form_dialog.is_none() {
            return;
        }

        let mut confirmed = false;
        let mut cancelled = false;
        egui::Modal::new(egui::Id::new("unsaved_form_dialog")).show(ui.ctx(), |ui| {
            ui.label("This form has unsaved changes. Continue and lose them?");
            ui.horizontal(|ui| {
                if ui.button("Continue").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            self.unsaved_form_dialog_confirmed();
        } else if cancelled {
            self.unsaved_form_dialog_cancelled();
        }
    }

    /// The "must validate before saving" prompt — opens when a `Save`/
    /// `SaveAs` comes back `SaveError::NotValidated` (see `apply_outcome`).
    /// `Asking` offers Validate/Cancel; `Validating` is a brief
    /// non-interactive "please wait" (gui-core answers fast enough that a
    /// spinner would be overkill — see README's Testing strategy on why
    /// this crate keeps such states simple); `Failed` swaps in the
    /// validation errors with a single "Ok" to close, no retry button —
    /// see `ValidateBeforeSaveDialogState`'s own doc comment on why.
    pub(crate) fn render_validate_before_save_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.validate_before_save_dialog.clone() else {
            return;
        };

        egui::Modal::new(egui::Id::new("validate_before_save_dialog")).show(ui.ctx(), |ui| {
            match state {
                ValidateBeforeSaveDialogState::Asking { .. } => {
                    ui.label(
                        "This project must be validated before it can be saved. Validate now?",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Validate").clicked() {
                            self.validate_before_save_confirmed();
                        }
                        if ui.button("Cancel").clicked() {
                            self.validate_before_save_dismissed();
                        }
                    });
                }
                ValidateBeforeSaveDialogState::Validating { .. } => {
                    ui.label("Validating…");
                }
                ValidateBeforeSaveDialogState::Failed { errors } => {
                    ui.label("Validation failed:");
                    for error in &errors {
                        ui.label(format!("\u{2022} {error}"));
                    }
                    if ui.button("Ok").clicked() {
                        self.validate_before_save_dismissed();
                    }
                }
            }
        });
    }

    pub(crate) fn render_load_error_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(message) = self.load_error_dialog.clone() else {
            return;
        };

        egui::Modal::new(egui::Id::new("load_error_dialog")).show(ui.ctx(), |ui| {
            ui.label("Couldn't open project:");
            ui.label(message);
            if ui.button("Ok").clicked() {
                self.load_error_dialog_dismissed();
            }
        });
    }

    pub(crate) fn render_exit_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.exit_dialog else {
            return;
        };

        egui::Modal::new(egui::Id::new("exit_dialog")).show(ui.ctx(), |ui| match state {
            ExitDialogState::Asking => {
                ui.label("You have unsaved changes. Save before exiting?");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.on_exit_dialog_save_clicked();
                    }
                    if ui.button("Discard").clicked() {
                        self.on_exit_dialog_discard_clicked();
                    }
                    if ui.button("Cancel").clicked() {
                        self.on_exit_dialog_cancel_clicked();
                    }
                });
            }
            ExitDialogState::Saving { .. } => {
                ui.label("Saving…");
            }
            ExitDialogState::TimedOut { .. } => {
                ui.label("Still saving — exit anyway and lose unsaved changes, or keep waiting?");
                ui.horizontal(|ui| {
                    if ui.button("Exit anyway").clicked() {
                        self.on_exit_dialog_exit_anyway_clicked();
                    }
                    if ui.button("Keep waiting").clicked() {
                        self.on_exit_dialog_keep_waiting_clicked();
                    }
                });
            }
            // `Ready` is consumed the same frame it's set (see
            // `take_ready_to_exit`, called before rendering in `ui()`), so
            // it's never observed here.
            ExitDialogState::Ready => {}
        });
    }

    pub(crate) fn render_attachments_dialog(&mut self, ui: &mut egui::Ui) {
        if self.attachments_dialog.is_none() {
            return;
        }

        let mut close_clicked = false;
        let mut add_attachment_clicked = false;
        let mut add_template_clicked = false;
        let mut remove_attachment: Option<std::path::PathBuf> = None;
        let mut remove_template: Option<std::path::PathBuf> = None;

        egui::Modal::new(egui::Id::new("attachments_dialog")).show(ui.ctx(), |ui| {
            let Some(dialog) = &mut self.attachments_dialog else {
                return;
            };
            ui.heading("Attachments");
            let module_label = if dialog.module.is_empty() {
                "(project root)".to_string()
            } else {
                dialog
                    .module
                    .iter()
                    .map(EntryName::as_str)
                    .collect::<Vec<_>>()
                    .join("/")
            };
            ui.label(format!("Module: {module_label}"));

            if dialog.loading {
                ui.label("Loading…");
            } else {
                ui.separator();
                ui.label("Attachments:");
                for path in &dialog.attachments {
                    ui.horizontal(|ui| {
                        ui.label(path.display().to_string());
                        if ui.button("Remove").clicked() {
                            remove_attachment = Some(path.clone());
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut dialog.new_attachment_path);
                    if ui.button("Add").clicked() {
                        add_attachment_clicked = true;
                    }
                });

                ui.separator();
                ui.label("Templates:");
                for path in &dialog.templates {
                    ui.horizontal(|ui| {
                        ui.label(path.display().to_string());
                        if ui.button("Remove").clicked() {
                            remove_template = Some(path.clone());
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut dialog.new_template_path);
                    if ui.button("Add").clicked() {
                        add_template_clicked = true;
                    }
                });
            }

            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }

            ui.separator();
            if ui.button("Close").clicked() {
                close_clicked = true;
            }
        });

        if let Some(path) = remove_attachment {
            self.attachments_dialog_remove_attachment_clicked(path);
        }
        if let Some(path) = remove_template {
            self.attachments_dialog_remove_template_clicked(path);
        }
        if add_attachment_clicked {
            self.attachments_dialog_add_attachment_clicked();
        }
        if add_template_clicked {
            self.attachments_dialog_add_template_clicked();
        }
        if close_clicked {
            self.attachments_dialog_closed();
        }
    }

    /// The "Commit all changes" modal — a multiline commit-message box
    /// (see `resizable_multiline`) plus a scrollable, depth-then-
    /// alphabetically sorted list of every path `GetChangedFiles` reported
    /// (already sorted by `apply_changed_files` — this just renders it in
    /// that order). Same bounded-`ScrollArea` shape as
    /// `render_path_picker_dialog` so a large changeset never grows the
    /// modal itself unbounded.
    pub(crate) fn render_commit_all_dialog(&mut self, ui: &mut egui::Ui) {
        if self.commit_all_dialog.is_none() {
            return;
        }

        let mut close_clicked = false;
        let mut commit_clicked = false;

        // Keep at least this many pixels between the modal's bottom edge and
        // the window edge — `egui::Modal` centers on the full screen but
        // never shrinks its frame to fit, so a long changed-files list near
        // the old fixed 300px cap could otherwise push the frame straight
        // past the viewport's bottom.
        const SCREEN_MARGIN: f32 = 10.0;
        // Rough, deliberately generous estimate of the popup frame's own
        // margin/shadow plus the heading/labels/separators/button row
        // surrounding the file list below — subtracted up front so the
        // *outer* modal border still clears `SCREEN_MARGIN` once that
        // chrome is added back around whatever we cap the file list to.
        const CHROME_BUFFER: f32 = 180.0;

        // Floor kept for the changed-files list once the message box has
        // taken whatever it wants — small enough to still show a couple of
        // rows, never fully squeezed out by a tall commit message.
        const FILE_LIST_MIN_HEIGHT: f32 = 60.0;

        let screen_rect = ui.ctx().content_rect();
        let resizable_budget =
            (screen_rect.height() - 2.0 * SCREEN_MARGIN - CHROME_BUFFER).max(200.0);
        // The message box can grow almost the whole budget — it's capped
        // only so the file list always keeps its floor — and whatever
        // height it actually ends up at (grown by content or dragged by
        // the user) is subtracted below to size the file list, so growing
        // the message box steals space from the file list instead of the
        // modal growing past the screen edge.
        let max_message_height = (resizable_budget - FILE_LIST_MIN_HEIGHT)
            .max(MULTILINE_ROW_HEIGHT * MULTILINE_MIN_ROWS as f32);
        let width = screen_rect.width() * 0.66;

        egui::Modal::new(egui::Id::new("commit_all_dialog")).show(ui.ctx(), |ui| {
            let Some(dialog) = &mut self.commit_all_dialog else {
                return;
            };
            ui.set_width(width);
            ui.heading("Commit all changes");

            if dialog.loading {
                ui.label("Loading…");
            } else {
                // Reserve exactly `resizable_budget` of vertical space for
                // the message box and file list together, then size the
                // file list to whatever's actually left in it once the
                // message box (and the labels/separator around it) have
                // really been laid out — measuring the live cursor this way
                // (instead of subtracting the message box's own reported
                // height from the budget) absorbs any of `Resize`'s own
                // chrome that isn't part of that reported height, so the
                // modal's total height stays constant as the message box is
                // dragged, rather than drifting by that leftover chrome.
                ui.allocate_ui_with_layout(
                    egui::vec2(width, resizable_budget),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_height(resizable_budget);
                        ui.label("Commit message:");
                        resizable_multiline_with_max_height(
                            ui,
                            "commit_all_message",
                            &mut dialog.message,
                            max_message_height,
                        );

                        ui.separator();
                        ui.label(format!("Changed files ({}):", dialog.changed_files.len()));
                        let max_file_list_height = ui.available_height().max(FILE_LIST_MIN_HEIGHT);
                        egui::ScrollArea::vertical()
                            .max_height(max_file_list_height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if dialog.changed_files.is_empty() {
                                    ui.label("No changes.");
                                } else {
                                    for path in &dialog.changed_files {
                                        ui.label(path.display().to_string());
                                    }
                                }
                            });
                    },
                );
            }

            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }

            ui.separator();
            ui.horizontal(|ui| {
                let can_commit = !dialog.loading
                    && !dialog.committing
                    && !dialog.changed_files.is_empty()
                    && !dialog.message.trim().is_empty();
                if ui
                    .add_enabled(
                        can_commit,
                        egui::Button::new(if dialog.committing {
                            "Committing…"
                        } else {
                            "Commit"
                        }),
                    )
                    .clicked()
                {
                    commit_clicked = true;
                }
                if ui.button("Cancel").clicked() {
                    close_clicked = true;
                }
            });
        });

        if commit_clicked {
            self.commit_all_dialog_commit_clicked();
        } else if close_clicked {
            self.commit_all_dialog_closed();
        }
    }

    /// The path-picker modal — see `PathPickerDialogState`'s own doc
    /// comment on why this replaced a per-field `egui::ComboBox`: a
    /// `ComboBox` popup sizes itself to its content with no scrolling, so
    /// a long enough list of requirements/tests would overflow off the
    /// screen with no way to search it down. This instead runs a real
    /// `ScrollArea` (bounded height, so the modal itself never grows
    /// unbounded either) over a filtered list, filtered by the same
    /// case-insensitive substring convention `node_matches_filter` already
    /// uses for the left pane's own tree filter.
    pub(crate) fn render_path_picker_dialog(&mut self, ui: &mut egui::Ui) {
        if self.path_picker_dialog.is_none() {
            return;
        }

        let mut cancel_clicked = false;
        let mut picked: Option<LogicalPath> = None;

        egui::Modal::new(egui::Id::new("path_picker_dialog")).show(ui.ctx(), |ui| {
            let (Some(dialog), Some(tree)) = (&mut self.path_picker_dialog, &self.tree) else {
                return;
            };
            ui.heading(match dialog.kind {
                EntryKind::Requirement => "Pick a requirement",
                EntryKind::Test => "Pick a test",
                EntryKind::Module | EntryKind::Result => {
                    unreachable!("no picker ever targets a module or result")
                }
            });
            ui.text_edit_singleline(&mut dialog.filter);
            let kind_segment = leaf_kind_segment(dialog.kind);
            let filter = dialog.filter.to_lowercase();

            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    let mut any_shown = false;
                    for target in flatten_leaf_paths(tree, dialog.kind) {
                        let path_str = absolute_reference_path(&target, kind_segment);
                        if !filter.is_empty() && !path_str.to_lowercase().contains(&filter) {
                            continue;
                        }
                        any_shown = true;
                        if ui.selectable_label(false, target.to_string()).clicked() {
                            picked = Some(target);
                        }
                    }
                    if !any_shown {
                        ui.label("No matches.");
                    }
                });

            ui.separator();
            if ui.button("Cancel").clicked() {
                cancel_clicked = true;
            }
        });

        if let Some(target) = picked {
            self.path_picker_dialog_selected(target);
        } else if cancel_clicked {
            self.path_picker_dialog_cancelled();
        }
    }

    #[cfg(all(feature = "debug-panel", debug_assertions))]
    pub(crate) fn render_debug_confirm_dialog(&mut self, ui: &mut egui::Ui) {
        if !self.debug.confirm_open {
            return;
        }

        let mut confirmed = false;
        let mut cancelled = false;
        egui::Modal::new(egui::Id::new("debug_confirm_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Open the debug panel?");
            ui.label(
                "It logs every message between the two threads and can trigger real \
                 stalls/failures — for development use, not normal use.",
            );
            ui.horizontal(|ui| {
                if ui.button("Open").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            self.debug_confirm_opened_clicked();
        } else if cancelled {
            self.debug_confirm_cancelled_clicked();
        }
    }

    /// The Delete-button confirmation prompt — opens from a Delete button
    /// on the requirement/test/result/module edit forms (never the
    /// project root's own page) and always asks before actually sending
    /// the `Command::Remove*`. See `DeleteConfirmState`'s own doc comment.
    pub(crate) fn render_delete_confirm_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(dialog) = self.delete_confirm_dialog.clone() else {
            return;
        };

        let mut confirmed = false;
        let mut cancelled = false;
        let busy = dialog.pending_request.is_some();
        egui::Modal::new(egui::Id::new("delete_confirm_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Delete?");
            ui.label(format!(
                "This will permanently delete \"{}\". This cannot be undone.",
                dialog.label
            ));
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Delete")).clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            self.delete_confirmed();
        } else if cancelled {
            self.delete_cancelled();
        }
    }

    /// The requirement "Recreate" prompt — opens from the "Recreate…"
    /// button next to a saved requirement's stable name. Cancel is only
    /// offered before the delete leg has gone out (`!dialog.deleted`):
    /// once the old requirement is actually gone, closing the dialog
    /// without finishing the create would just lose it, so at that point
    /// entering a name and clicking "Recreate" (to retry the create) is
    /// the only way out other than the name field going empty being
    /// rejected outright. See `RecreateRequirementState`'s own doc
    /// comment.
    pub(crate) fn render_recreate_requirement_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(dialog) = self.recreate_requirement_dialog.clone() else {
            return;
        };

        let mut new_name = dialog.new_name;
        let mut confirmed = false;
        let mut cancelled = false;
        let busy = dialog.pending_request.is_some();
        egui::Modal::new(egui::Id::new("recreate_requirement_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Recreate Requirement");
            ui.label(format!(
                "This deletes \"{}\" and creates a new requirement with the same contents under a new stable name.",
                dialog.target.name
            ));
            ui.horizontal(|ui| {
                ui.label("New name:");
                ui.text_edit_singleline(&mut new_name);
            });
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy && !new_name.trim().is_empty(), egui::Button::new("Recreate"))
                    .clicked()
                {
                    confirmed = true;
                }
                if ui.add_enabled(!busy && !dialog.deleted, egui::Button::new("Cancel")).clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            if let Some(dialog) = &mut self.recreate_requirement_dialog {
                dialog.new_name = new_name;
            }
            self.recreate_requirement_confirmed();
        } else if cancelled {
            self.recreate_requirement_cancelled();
        } else if let Some(dialog) = &mut self.recreate_requirement_dialog {
            dialog.new_name = new_name;
        }
    }

    pub(crate) fn render_recreate_test_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(dialog) = self.recreate_test_dialog.clone() else {
            return;
        };

        let mut new_name = dialog.new_name;
        let mut confirmed = false;
        let mut cancelled = false;
        let busy = dialog.pending_request.is_some();
        egui::Modal::new(egui::Id::new("recreate_test_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Recreate Test");
            ui.label(format!(
                "This deletes \"{}\" and creates a new test with the same contents under a new stable name.",
                dialog.target.name
            ));
            ui.horizontal(|ui| {
                ui.label("New name:");
                ui.text_edit_singleline(&mut new_name);
            });
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy && !new_name.trim().is_empty(), egui::Button::new("Recreate"))
                    .clicked()
                {
                    confirmed = true;
                }
                if ui.add_enabled(!busy && !dialog.deleted, egui::Button::new("Cancel")).clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            if let Some(dialog) = &mut self.recreate_test_dialog {
                dialog.new_name = new_name;
            }
            self.recreate_test_confirmed();
        } else if cancelled {
            self.recreate_test_cancelled();
        } else if let Some(dialog) = &mut self.recreate_test_dialog {
            dialog.new_name = new_name;
        }
    }

    #[cfg(all(feature = "debug-panel", debug_assertions))]
    pub(crate) fn render_debug_panel(&mut self, ui: &mut egui::Ui) {
        if !self.debug.open {
            return;
        }

        egui::Panel::right("debug_panel")
            .default_size(320.0)
            .size_range(240.0..=600.0)
            .show(ui, |ui| {
                ui.heading("Debug");

                ui.separator();
                ui.label("Local gui-ui state:");
                ui.label(format!("pending: {}", self.pending.len()));
                ui.label(format!("dirty: {}", self.dirty));
                ui.label(format!("selection: {:?}", self.selection));
                ui.label(format!("selected_module: {:?}", self.selected_module));
                ui.label(format!("project_path: {:?}", self.project_path));
                ui.label(format!(
                    "nav_history: {} entries, position {}",
                    self.nav_history.len(),
                    self.nav_position
                ));
                ui.label(format!("exit_dialog: {:?}", self.exit_dialog));

                ui.separator();
                ui.label("Trigger:");
                ui.horizontal(|ui| {
                    if ui.button("Tx Stall").clicked() {
                        self.debug.trigger_tx_stall(std::time::Instant::now());
                    }
                    if ui.button("Tx Failure").clicked() {
                        self.debug.trigger_tx_failure();
                    }
                    if ui.button("Rx Stall").clicked() {
                        self.debug.trigger_rx_stall(std::time::Instant::now());
                    }
                });
                if self.debug.is_tx_stalled() {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Tx is currently stalled — commands are queuing.",
                    );
                }
                if self.debug.is_rx_stalled(std::time::Instant::now()) {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Rx is currently stalled — events are queuing.",
                    );
                }
                // No "Rx Failure" button — a genuine one (an `Event` `gui-core`
                // computed but never sent) needs real `gui-core` cooperation
                // to reproduce honestly, which isn't built yet; see README's
                // "Planned: debug side panel" for the open decision on
                // whether that's worth adding to `gui-core`'s production
                // `Command` enum for a purely diagnostic feature.
                ui.label("(Rx Failure not implemented — see README)");

                ui.separator();
                ui.label(format!(
                    "Message log ({} entries, oldest first):",
                    self.debug.log.len()
                ));
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for entry in &self.debug.log {
                            let prefix = match entry.direction {
                                crate::debug_panel::LogDirection::Tx => "→ Tx",
                                crate::debug_panel::LogDirection::TxDropped => "✗ Tx dropped",
                                crate::debug_panel::LogDirection::Rx => "← Rx",
                            };
                            // `at.elapsed()` — recomputed fresh every frame from
                            // when this entry was actually logged, rather than a
                            // value baked in once, so "how long ago" keeps
                            // ticking up correctly while the panel stays open.
                            ui.label(format!(
                                "[{:>6.1}s] {prefix}: {}",
                                entry.at.elapsed().as_secs_f32(),
                                entry.detail
                            ));
                        }
                    });
            });
    }
}

/// Only ever called for a `Module` node — `render_module_children` filters
/// to `EntryKind::Module` before recursing here, since the top tree pane
/// no longer renders leaves at all.
fn render_tree_node(
    app: &mut GuiApp,
    ui: &mut egui::Ui,
    node: &TreeNode,
    module_path: &[EntryName],
    force_open: Option<bool>,
) {
    // A module with no matching descendant module (filter active) is
    // skipped entirely, not just collapsed — see `module_matches_filter`'s
    // own doc comment.
    if !module_matches_filter(node, module_path, &app.tree_filter) {
        return;
    }

    let mut this_module_path = module_path.to_vec();
    this_module_path.push(node.name.clone());
    let is_current = app.selected_module == this_module_path;

    ui.horizontal(|ui| {
        // A module has no `EntryDetail`/form of its own (see that type's
        // doc comment) — this button is the only way to make it the
        // "current module" new entries and the Attachments dialog target;
        // the CollapsingHeader label itself only toggles expand/collapse.
        let glyph = if is_current {
            icons::MODULE_CURRENT
        } else {
            icons::MODULE_NOT_CURRENT
        };
        let mut text = egui::RichText::new(glyph);
        if is_current {
            text = text.color(theme_colors::module_current_color(ui.visuals().dark_mode));
        }
        if ui
            .add(egui::Button::new(text).small())
            .on_hover_text("Set as current module")
            .clicked()
        {
            if app.editor_has_unsaved_edits() {
                app.unsaved_form_dialog_opened(PendingNavigation::SelectModule(
                    this_module_path.clone(),
                ));
            } else {
                app.select_module(this_module_path.clone());
            }
        }
        egui::CollapsingHeader::new(node.name.as_str())
            .default_open(false)
            .open(force_open)
            .show(ui, |ui| {
                render_module_children(app, ui, &node.children, &this_module_path, force_open);
            });
    });
}

/// Renders one module's submodules — the top tree pane is a pure module
/// hierarchy now, so unlike its previous shape this no longer also draws
/// that module's own requirement/test/result leaves (those belong to the
/// selected-module pane below the separator; see
/// `render_selected_module_pane`).
fn render_module_children(
    app: &mut GuiApp,
    ui: &mut egui::Ui,
    children: &[TreeNode],
    module_path: &[EntryName],
    force_open: Option<bool>,
) {
    for child in children {
        if child.kind == EntryKind::Module {
            render_tree_node(app, ui, child, module_path, force_open);
        }
    }
}

/// One collapsible folder ("requirements"/"tests"/"results") holding
/// every child of `kind` — omitted entirely when there are none, so an
/// empty module doesn't grow three empty, useless folders.
///
/// The header shows how many of `kind` live directly in this module (the
/// same count as `matching.len()` below — filtered by `tree_filter`, like
/// the rows themselves) and, only when this module actually has
/// submodules (`recursive_total.is_some()`), a second, unfiltered total
/// covering this module and everything under it — the "how many are
/// there really" number the module-only top tree can't answer on its own
/// since it never shows leaves.
fn render_leaf_group(
    app: &mut GuiApp,
    ui: &mut egui::Ui,
    title: &str,
    kind: EntryKind,
    children: &[TreeNode],
    module_path: &[EntryName],
    display: LeafGroupDisplay,
) {
    let matching: Vec<&TreeNode> = children
        .iter()
        .filter(|child| {
            child.kind == kind && node_matches_filter(child, module_path, &app.tree_filter)
        })
        .collect();
    if matching.is_empty() {
        return;
    }
    let header = match display.recursive_total {
        Some(total) => format!("{title} ({} · {total} total)", matching.len()),
        None => format!("{title} ({})", matching.len()),
    };
    // The header text carries a match count that changes as the filter
    // bar is typed into — an explicit `id_salt` (independent of that
    // text) keeps this header's open/closed state stable across those
    // changes. Without it, `CollapsingHeader` falls back to hashing the
    // label itself as its persistent id (see its own doc comment), so
    // every count change would silently re-collapse an already-open
    // group back to `default_open(false)`, hiding leaves that still
    // match the filter.
    egui::CollapsingHeader::new(header)
        .id_salt((title, module_path))
        .default_open(false)
        .open(display.force_open)
        .show(ui, |ui| {
            for leaf in matching {
                render_leaf(app, ui, leaf, module_path);
            }
        });
}

/// The two per-frame, per-group settings `render_leaf_group` needs beyond
/// its `TreeNode` data — bundled together so the function stays under
/// clippy's argument-count lint.
#[derive(Clone, Copy)]
struct LeafGroupDisplay {
    force_open: Option<bool>,
    recursive_total: Option<usize>,
}

/// Counts every descendant of `kind` under `node`, including `node`'s own
/// direct children — the module-recursive total `render_leaf_group`'s
/// header shows alongside the (possibly filtered) count of just this
/// module's own children.
fn count_kind_recursive(node: &TreeNode, kind: EntryKind) -> usize {
    node.children
        .iter()
        .map(|child| {
            if child.kind == EntryKind::Module {
                count_kind_recursive(child, kind)
            } else {
                usize::from(child.kind == kind)
            }
        })
        .sum()
}

fn render_leaf(app: &mut GuiApp, ui: &mut egui::Ui, node: &TreeNode, module_path: &[EntryName]) {
    // Both arms need to end up the same type for the one shared
    // `selectable_label` call below — `Atoms` is that common type (a
    // requirement's colored icon + plain name is a 2-`Atom` tuple, every
    // other kind's bare name is a 1-`Atom` string; `.into_atoms()`
    // unifies them, see `egui::IntoAtoms`).
    use egui::IntoAtoms as _;
    let content = match node.kind {
        EntryKind::Requirement => {
            let (fg, bg) = crate::theme_colors::status_colors(ui.visuals().dark_mode, node.status);
            let icon = crate::icons::status_icon(node.status);
            (
                egui::RichText::new(icon).color(fg).background_color(bg),
                node.name.as_str().to_string(),
            )
                .into_atoms()
        }
        _ => node.name.as_str().to_string().into_atoms(),
    };
    if ui.selectable_label(false, content).clicked() {
        let target = LogicalPath {
            modules: module_path.to_vec(),
            name: node.name.clone(),
        };
        if app.editor_has_unsaved_edits() {
            app.unsaved_form_dialog_opened(PendingNavigation::Select {
                target,
                kind: node.kind,
            });
        } else {
            app.select(target, node.kind);
        }
    }
}

/// Walks `root` by `path`, matching only `Module` children at each
/// segment — the `TreeNode` counterpart of `gui-core`'s own
/// `resolve_module` (which walks a `ModuleDraft`, not the simplified
/// read-model tree gui-ui already has in hand each frame). An empty
/// `path` returns `root` itself, the same "empty means project root"
/// convention `selected_module` uses.
fn resolve_tree_module<'a>(root: &'a TreeNode, path: &[EntryName]) -> Option<&'a TreeNode> {
    let mut current = root;
    for name in path {
        current = current
            .children
            .iter()
            .find(|child| child.kind == EntryKind::Module && child.name == *name)?;
    }
    Some(current)
}

/// The bottom half of the tree pane: the requirements/tests/results,
/// attachments, and templates belonging to `app.selected_module` (or the
/// project root, if empty) — not its submodules', and not the whole
/// project's. Mirrors `render_module_children`'s old (pre-split)
/// requirement/test/result grouping, but against exactly one module's own
/// `children` instead of recursing into every module in the tree.
fn render_selected_module_pane(
    app: &mut GuiApp,
    ui: &mut egui::Ui,
    tree: &TreeSnapshot,
    force_open: Option<bool>,
) {
    let module_path = app.selected_module.clone();
    let Some(node) = resolve_tree_module(&tree.root, &module_path) else {
        // Can happen if the selected module was just deleted out from
        // under the selection.
        ui.label("Selected module no longer exists.");
        return;
    };
    let has_submodules = node
        .children
        .iter()
        .any(|child| child.kind == EntryKind::Module);
    let requirement_total =
        has_submodules.then(|| count_kind_recursive(node, EntryKind::Requirement));
    let test_total = has_submodules.then(|| count_kind_recursive(node, EntryKind::Test));
    let result_total = has_submodules.then(|| count_kind_recursive(node, EntryKind::Result));
    let children = node.children.clone();

    render_leaf_group(
        app,
        ui,
        "requirements",
        EntryKind::Requirement,
        &children,
        &module_path,
        LeafGroupDisplay {
            force_open,
            recursive_total: requirement_total,
        },
    );
    render_leaf_group(
        app,
        ui,
        "tests",
        EntryKind::Test,
        &children,
        &module_path,
        LeafGroupDisplay {
            force_open,
            recursive_total: test_total,
        },
    );
    render_leaf_group(
        app,
        ui,
        "results",
        EntryKind::Result,
        &children,
        &module_path,
        LeafGroupDisplay {
            force_open,
            recursive_total: result_total,
        },
    );

    if let Some(pools) = app.sidebar_pools.clone() {
        render_pool_group(ui, "attachments", &pools.attachments);
        render_pool_group(ui, "templates", &pools.templates);
    }
}

/// A read-only listing of `paths` under a collapsible `title` folder —
/// omitted entirely when empty, same convention as `render_leaf_group`.
/// Unlike a leaf row, these aren't clickable: attachments/templates have
/// no `EntryDetail`/form of their own to navigate to here (the
/// Attachments modal is still where they're added/removed).
fn render_pool_group(ui: &mut egui::Ui, title: &str, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(title)
        .default_open(false)
        .show(ui, |ui| {
            for path in paths {
                ui.label(path.display().to_string());
            }
        });
}

/// Three radio buttons switching `dep`'s variant — resets its fields to
/// empty rather than trying to carry any over, since a `LocalRequirement`'s
/// `path`/`Remote`'s `url` mean different things (see `DependencyDraft`'s
/// own doc comment). Shared by an existing dependency's own row and the
/// "Add dependency" composer in `render_requirement_form`.
/// Returns whether `dep`'s variant was actually switched — the caller
/// decides what that means (an existing row's `edited` flag flips;
/// the "Add dependency" composer's scratch entry doesn't, since nothing
/// real has changed until it's actually added — see both call sites in
/// `render_requirement_form`).
fn render_dependency_kind_picker(ui: &mut egui::Ui, dep: &mut DependencyDraft) -> bool {
    if ui
        .radio(
            matches!(dep, DependencyDraft::LocalRequirement { .. }),
            "Local",
        )
        .clicked()
    {
        *dep = DependencyDraft::LocalRequirement {
            path: String::new(),
            commit: String::new(),
        };
        return true;
    }
    if ui
        .radio(matches!(dep, DependencyDraft::Remote { .. }), "Remote")
        .clicked()
    {
        *dep = DependencyDraft::Remote {
            url: String::new(),
            path: String::new(),
            commit: String::new(),
        };
        return true;
    }
    if ui
        .radio(matches!(dep, DependencyDraft::Submodules), "Submodules")
        .clicked()
    {
        *dep = DependencyDraft::Submodules;
        return true;
    }
    false
}

/// `dep`'s own editable fields, per variant — `Submodules` has none.
/// Shared the same way `render_dependency_kind_picker` is, including its
/// `changed` return-value convention (the first element of the tuple).
///
/// The second element reports an "Auto" commit-fetch click, if one
/// happened this frame — the caller (which owns `self`, unlike this free
/// function) turns it into an actual `Command` via
/// `GuiApp::dependency_commit_auto_clicked`, same "capture during
/// rendering, act after the borrow of `self.editor` ends" split every
/// other button here already follows. `tree` drives both the `Local`
/// variant's path picker (same "picker alongside a still-hand-editable
/// text field" shape as the Result form's own pickers — see
/// `absolute_reference_path`'s doc comment) and its "Auto" button, which
/// resolves the *typed* path against `tree`'s own entries to find a
/// `LogicalPath` to resolve a commit for — so Auto only works once the
/// field holds a path that actually matches something in the loaded tree
/// (picked from the picker modal, or hand-typed correctly), same
/// limitation the Result form's pickers already have with a stale/
/// unloaded tree.
///
/// The third element of the returned tuple reports a "Pick…" click —
/// like `auto`, the caller (which knows whether this is an existing row
/// or the composer, and so which `DependencySlot`/`PathPickerTarget` it
/// means) turns it into `GuiApp::path_picker_dialog_opened` after the
/// borrow of `self.editor` ends.
fn render_dependency_fields(
    ui: &mut egui::Ui,
    dep: &mut DependencyDraft,
    tree: Option<&TreeSnapshot>,
) -> (bool, Option<AutoCommitKind>, bool) {
    match dep {
        DependencyDraft::LocalRequirement { path, commit } => {
            let mut changed = false;
            let mut auto = None;
            let mut pick_clicked = false;
            ui.horizontal(|ui| {
                ui.label("Path:");
                changed |= ui.text_edit_singleline(path).changed();
                if tree.is_some() && ui.button("Pick…").clicked() {
                    pick_clicked = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Commit:");
                changed |= ui.text_edit_singleline(commit).changed();
                if ui.button("Auto").clicked()
                    && let Some(tree) = tree
                    && let Some(target) = flatten_leaf_paths(tree, EntryKind::Requirement)
                        .into_iter()
                        .find(|target| absolute_reference_path(target, "requirements") == *path)
                {
                    auto = Some(AutoCommitKind::Local(target));
                }
            });
            (changed, auto, pick_clicked)
        }
        DependencyDraft::Remote { url, path, commit } => {
            let mut changed = false;
            let mut auto = None;
            ui.horizontal(|ui| {
                ui.label("URL:");
                changed |= ui.text_edit_singleline(url).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Path (optional):");
                changed |= ui.text_edit_singleline(path).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Commit:");
                changed |= ui.text_edit_singleline(commit).changed();
                if ui.button("Auto").clicked() && !url.trim().is_empty() {
                    auto = Some(AutoCommitKind::Remote {
                        url: url.clone(),
                        path: if path.trim().is_empty() {
                            None
                        } else {
                            Some(ReferencePath(path.clone()))
                        },
                    });
                }
            });
            (changed, auto, false)
        }
        DependencyDraft::Submodules => (false, None, false),
    }
}

/// `test_ref`'s own editable fields — a `TestRefDraft` only ever has one
/// shape (`path`/`commit`, like `DependencyDraft::LocalRequirement`), so
/// unlike dependencies there's no kind picker. Same return-value and
/// "capture during rendering, act after the borrow of `self.editor` ends"
/// conventions as `render_dependency_fields`: `changed` for a plain text
/// edit, `auto` for an "Auto" commit-fetch click (turned into a
/// `GuiApp::test_ref_commit_auto_clicked` call by the caller), `pick_clicked`
/// for a "Pick…" click (turned into `GuiApp::path_picker_dialog_opened`).
fn render_test_ref_fields(
    ui: &mut egui::Ui,
    test_ref: &mut TestRefDraft,
    tree: Option<&TreeSnapshot>,
) -> (bool, Option<LogicalPath>, bool) {
    let mut changed = false;
    let mut auto = None;
    let mut pick_clicked = false;
    ui.horizontal(|ui| {
        ui.label("Path:");
        changed |= ui.text_edit_singleline(&mut test_ref.path).changed();
        if tree.is_some() && ui.button("Pick…").clicked() {
            pick_clicked = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label("Commit:");
        changed |= ui.text_edit_singleline(&mut test_ref.commit).changed();
        if ui.button("Auto").clicked()
            && let Some(tree) = tree
            && let Some(target) =
                flatten_leaf_paths(tree, EntryKind::Test)
                    .into_iter()
                    .find(|target| {
                        absolute_reference_path(target, leaf_kind_segment(EntryKind::Test))
                            == test_ref.path
                    })
        {
            auto = Some(target);
        }
    });
    (changed, auto, pick_clicked)
}

/// Whether `node` (a leaf or a module) should be visible under the left
/// pane's filter bar — `true` unconditionally when `filter` is empty
/// (the unfiltered, "show everything" case), otherwise a case-
/// insensitive substring match. A leaf matches when its own
/// fully-qualified logical path (the same
/// `/[modules/<sub>/]*<kind>/<name>` shape `absolute_reference_path`
/// builds for the Result form's pickers, e.g. `/requirements/definition`
/// or `/modules/setup/tests/generic_test`) contains `filter`. A module
/// matches when *any* descendant leaf, at any depth, matches — so a
/// module containing a single matching leaf three levels down still
/// shows (collapsed headers and all the way up to the root), while one
/// with no matching descendant at all is skipped entirely rather than
/// shown empty. `module_path` is the path *to* `node`, same convention
/// every other tree-rendering function here uses (does not include
/// `node.name` itself for a module — the caller pushes that before
/// recursing, this function does the pushing internally when walking
/// `node`'s own children).
/// Whether `node` (always a `Module` — the top tree pane no longer renders
/// leaves) should be visible under the left pane's filter bar. `true`
/// unconditionally when `filter` is empty, otherwise true when this
/// module's own fully-qualified path (`module_path` + `node.name`,
/// lowercased) contains `filter`, or any submodule matches recursively —
/// same "show the path to a match, skip everything with no match at all"
/// shape `node_matches_filter` used for the old combined tree, just scored
/// against module names instead of leaf paths now that leaves live in a
/// separate pane.
fn module_matches_filter(node: &TreeNode, module_path: &[EntryName], filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_lowercase();
    let mut this_module_path = module_path.to_vec();
    this_module_path.push(node.name.clone());
    let full_path = this_module_path
        .iter()
        .map(EntryName::as_str)
        .collect::<Vec<_>>()
        .join("/")
        .to_lowercase();
    if full_path.contains(&filter) {
        return true;
    }
    node.children
        .iter()
        .filter(|child| child.kind == EntryKind::Module)
        .any(|child| module_matches_filter(child, &this_module_path, &filter))
}

/// Whether `node` (a leaf or a module) should be visible under the left
/// pane's filter bar — `true` unconditionally when `filter` is empty
/// (the unfiltered, "show everything" case), otherwise a case-
/// insensitive substring match. A leaf matches when its own
/// fully-qualified logical path (the same
/// `/[modules/<sub>/]*<kind>/<name>` shape `absolute_reference_path`
/// builds for the Result form's pickers, e.g. `/requirements/definition`
/// or `/modules/setup/tests/generic_test`) contains `filter`. A module
/// matches when *any* descendant leaf, at any depth, matches — so a
/// module containing a single matching leaf three levels down still
/// shows (collapsed headers and all the way up to the root), while one
/// with no matching descendant at all is skipped entirely rather than
/// shown empty. `module_path` is the path *to* `node`, same convention
/// every other tree-rendering function here uses (does not include
/// `node.name` itself for a module — the caller pushes that before
/// recursing, this function does the pushing internally when walking
/// `node`'s own children).
fn node_matches_filter(node: &TreeNode, module_path: &[EntryName], filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_lowercase();
    match node.kind {
        EntryKind::Module => {
            let mut this_module_path = module_path.to_vec();
            this_module_path.push(node.name.clone());
            node.children
                .iter()
                .any(|child| node_matches_filter(child, &this_module_path, &filter))
        }
        leaf_kind => {
            let target = LogicalPath {
                modules: module_path.to_vec(),
                name: node.name.clone(),
            };
            absolute_reference_path(&target, leaf_kind_segment(leaf_kind))
                .to_lowercase()
                .contains(&filter)
        }
    }
}

#[cfg(test)]
mod test {
    use gui_core::EntryStatus;

    use super::*;

    fn name(name: &str) -> EntryName {
        EntryName(name.to_string())
    }

    fn leaf(kind: EntryKind, name_str: &str) -> TreeNode {
        TreeNode {
            name: name(name_str),
            kind,
            status: EntryStatus::Unvalidated,
            children: Vec::new(),
        }
    }

    fn module(name_str: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            name: name(name_str),
            kind: EntryKind::Module,
            status: EntryStatus::Unvalidated,
            children,
        }
    }

    #[test]
    fn absolute_reference_path_for_a_root_level_entry_has_no_module_segments() {
        let target = LogicalPath::root(name("definition"));
        assert_eq!(
            absolute_reference_path(&target, "requirements"),
            "/requirements/definition"
        );
    }

    #[test]
    fn absolute_reference_path_for_a_nested_entry_includes_every_module_segment() {
        let target = LogicalPath {
            modules: vec![name("setup"), name("nested")],
            name: name("generic_test"),
        };
        assert_eq!(
            absolute_reference_path(&target, "tests"),
            "/modules/setup/modules/nested/tests/generic_test"
        );
    }

    #[test]
    fn flatten_leaf_paths_skips_the_root_display_name_and_finds_root_level_entries() {
        let tree = TreeSnapshot {
            // The root's own `name` ("Capstone", say) must never leak into
            // a child's path — see this function's own doc comment.
            root: module(
                "Capstone",
                vec![
                    leaf(EntryKind::Requirement, "definition"),
                    leaf(EntryKind::Test, "generic_test"),
                ],
            ),
            can_undo: false,
            can_redo: false,
        };

        let requirements = flatten_leaf_paths(&tree, EntryKind::Requirement);

        assert_eq!(requirements, vec![LogicalPath::root(name("definition"))]);
    }

    #[test]
    fn flatten_leaf_paths_walks_into_nested_modules() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![module(
                    "setup",
                    vec![leaf(EntryKind::Requirement, "nested_requirement")],
                )],
            ),
            can_undo: false,
            can_redo: false,
        };

        let requirements = flatten_leaf_paths(&tree, EntryKind::Requirement);

        assert_eq!(
            requirements,
            vec![LogicalPath {
                modules: vec![name("setup")],
                name: name("nested_requirement"),
            }]
        );
    }

    #[test]
    fn flatten_leaf_paths_only_returns_the_requested_kind() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![
                    leaf(EntryKind::Requirement, "definition"),
                    leaf(EntryKind::Result, "definition"),
                ],
            ),
            can_undo: false,
            can_redo: false,
        };

        assert_eq!(flatten_leaf_paths(&tree, EntryKind::Test), Vec::new());
    }

    #[test]
    fn flatten_leaf_paths_orders_shallower_modules_before_deeper_ones() {
        // "zzz_root_level" sorts after "nested" alphabetically, so this
        // only passes if depth — not tree-walk/name order — decides the
        // result: the picker should list the project root's own entries,
        // then a module's, before a submodule's, regardless of naming.
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![
                    module(
                        "nested",
                        vec![module(
                            "deeper",
                            vec![leaf(EntryKind::Requirement, "deepest")],
                        )],
                    ),
                    leaf(EntryKind::Requirement, "zzz_root_level"),
                ],
            ),
            can_undo: false,
            can_redo: false,
        };

        let requirements = flatten_leaf_paths(&tree, EntryKind::Requirement);

        assert_eq!(
            requirements,
            vec![
                LogicalPath::root(name("zzz_root_level")),
                LogicalPath {
                    modules: vec![name("nested"), name("deeper")],
                    name: name("deepest"),
                },
            ]
        );
    }

    #[test]
    fn an_empty_filter_matches_every_node() {
        let node = leaf(EntryKind::Requirement, "definition");
        assert!(node_matches_filter(&node, &[], ""));
    }

    #[test]
    fn a_leaf_matches_a_substring_of_its_own_absolute_path_case_insensitively() {
        let node = leaf(EntryKind::Requirement, "definition");
        assert!(node_matches_filter(&node, &[], "REQUIREMENTS/DEF"));
    }

    #[test]
    fn a_leaf_does_not_match_a_substring_absent_from_its_absolute_path() {
        let node = leaf(EntryKind::Requirement, "definition");
        assert!(!node_matches_filter(&node, &[], "nonexistent"));
    }

    #[test]
    fn a_module_matches_when_a_descendant_at_any_depth_matches() {
        let tree = module(
            "setup",
            vec![module(
                "nested",
                vec![leaf(EntryKind::Test, "generic_test")],
            )],
        );
        assert!(node_matches_filter(&tree, &[], "generic_test"));
    }

    #[test]
    fn a_module_does_not_match_when_no_descendant_matches() {
        let tree = module("setup", vec![leaf(EntryKind::Test, "generic_test")]);
        assert!(!node_matches_filter(&tree, &[], "nonexistent"));
    }

    #[test]
    fn an_empty_module_never_matches_a_non_empty_filter() {
        let tree = module("setup", Vec::new());
        assert!(!node_matches_filter(&tree, &[], "setup"));
    }
}
