//! Rendering only — every decision (what a click *means*) lives in
//! `lib.rs`'s plain methods (`on_exit_clicked`, `select`, ...); this file
//! just draws widgets and calls them. See README's "Logic: keep it out of
//! `update()`" and "Layout".

use std::path::PathBuf;

use gui_core::{EntryKind, EntryName, EntryStatus, LogicalPath, ResultKindV1, TreeNode, TreeSnapshot};

use crate::{DependencyDraft, EditorState, ExitDialogState, GuiApp, LocalPoolKind, PendingNavigation, PendingProjectAction};

/// Pops a native OS folder picker (`rfd`) titled `title` — blocking, but
/// bounded by the user's own interaction with it, not by anything
/// gui-core does; see README's "Never block the render thread" for why
/// that's a deliberate, documented exception rather than a violation of
/// it. Shared by Open Project's own not-dirty click and its
/// confirmed-after-unsaved-changes resume path (`render_unsaved_changes_dialog`).
fn pick_project_folder(title: &str) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).pick_folder()
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
            if let Some(path) = rfd::FileDialog::new().set_title("Save Project As").pick_folder() {
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
                    if ui.button("New Project…").clicked() {
                        if self.dirty {
                            self.unsaved_changes_dialog_opened(PendingProjectAction::NewProject);
                        } else {
                            self.new_project_dialog_opened();
                        }
                        ui.close();
                    }
                    if ui.button("Open Project…").clicked() {
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
                                        self.unsaved_changes_dialog_opened(PendingProjectAction::OpenRecent(path));
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
                    if ui.add_enabled(has_project, egui::Button::new("Save")).clicked() {
                        self.save_button_clicked();
                        ui.close();
                    }
                    if ui.add_enabled(has_project, egui::Button::new("Save As…")).clicked() {
                        if let Some(path) = rfd::FileDialog::new().set_title("Save Project As").pick_folder() {
                            self.save_project_as(path);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
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
                    let label = if self.debug.open { "Debug ▲" } else { "Debug ▼" };
                    if ui.button(label).clicked() {
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
                // File menu's own Save item, see `render_menu_bar`.
                if ui.add_enabled(self.tree.is_some(), egui::Button::new("Save")).clicked() {
                    self.save_button_clicked();
                }
                if ui.button("Validate").clicked() {
                    self.validate_clicked();
                }
                ui.separator();
                // `can_undo`/`can_redo` come from `self.tree` (`gui-core`'s
                // own bookkeeping, piggybacked on `TreeSnapshot` — see
                // that type's own doc comment), not tracked locally —
                // disabled with nothing loaded at all, same as every
                // button here that needs a real project underneath it.
                let can_undo = self.tree.as_ref().is_some_and(|tree| tree.can_undo);
                if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
                    self.undo_clicked();
                }
                let can_redo = self.tree.as_ref().is_some_and(|tree| tree.can_redo);
                if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
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
                if ui.add_enabled(self.can_go_back(), egui::Button::new("Back")).clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::Back);
                    } else {
                        self.back_clicked();
                    }
                }
                if ui.add_enabled(self.can_go_forward(), egui::Button::new("Forward")).clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::Forward);
                    } else {
                        self.forward_clicked();
                    }
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    self.on_exit_clicked();
                }
                ui.separator();
                if ui.button("New Requirement").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewRequirement);
                    } else {
                        self.new_requirement_clicked();
                    }
                }
                if ui.button("New Test").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewTest);
                    } else {
                        self.new_test_clicked();
                    }
                }
                if ui.button("New Result").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewResult);
                    } else {
                        self.new_result_clicked();
                    }
                }
                if ui.button("New Module").clicked() {
                    if self.editor_has_unsaved_edits() {
                        self.unsaved_form_dialog_opened(PendingNavigation::NewModule);
                    } else {
                        self.new_module_clicked();
                    }
                }
                ui.separator();
                if ui.button("Attachments…").clicked() {
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
                    ui.label("● unsaved changes");
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

                // Zoom controls, pinned to the status bar's far right —
                // added in reverse (`+` first) since `right_to_left`
                // places each new widget further left of the last,
                // starting from the right edge; this order reads
                // "Reset − [value]% +" left-to-right, `+`/`−` bracketing
                // the editable value per the usual zoom-control
                // convention, Reset furthest left since it's the least
                // frequently used of the four.
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
                    let response = ui.add(egui::TextEdit::singleline(&mut self.zoom_input).desired_width(30.0));
                    if response.lost_focus() {
                        self.zoom_input_submitted();
                    }
                    if ui.button("−").clicked() {
                        self.zoom_out_clicked();
                    }
                    if ui.button("Reset").clicked() {
                        self.zoom_reset_clicked();
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
            ui.separator();

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match &self.tree {
                None => {
                    ui.label("No project loaded.");
                }
                Some(tree) => {
                    let root = tree.root.clone();
                    // The root `TreeNode`'s own `name` is the project's
                    // display name (see `gui-core`'s `build_tree_snapshot`)
                    // — a label, not a real module-path segment. Unlike
                    // every other `Module` node, it must never be pushed
                    // into a path, so the root is rendered specially here
                    // (its own selector + iterating its children with an
                    // *empty* path) rather than through `render_tree_node`,
                    // which pushes `node.name` for every module it handles.
                    let is_root_current = self.selected_module.is_empty();
                    ui.horizontal(|ui| {
                        let glyph = if is_root_current { "◉" } else { "○" };
                        if ui.small_button(glyph).on_hover_text("Set as current module").clicked() {
                            self.select_module(Vec::new());
                        }
                        ui.strong(root.name.as_str());
                    });
                    render_module_children(self, ui, &root.children, &[]);
                }
            });
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
        }
        let pane = match &self.editor {
            EditorState::None => Pane::Empty,
            EditorState::NewRequirement(_) => Pane::NewRequirement,
            EditorState::NewTest(_) => Pane::NewTest,
            EditorState::NewResult(_) => Pane::NewResult,
            EditorState::NewModule(_) => Pane::NewModule,
        };

        egui::CentralPanel::default().show(ui, |ui| {
            // A form can run longer than the window is tall (a
            // requirement with several dependencies and local
            // attachments, say) — without this, content past the bottom
            // edge is simply unreachable, Save/Cancel included, with no
            // visible sign anything's missing (same overflow-past-
            // viewport bug class as the toolbar/zoom/filter-field fixes
            // — see README's Testing strategy). `auto_shrink([false,
            // false])` for the same reason the left pane's own
            // `ScrollArea` needs it: without it, the area shrinks to fit
            // its content's natural size and never actually scrolls.
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match pane {
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
            });
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
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        {
            let EditorState::NewRequirement(form) = &mut self.editor else {
                return;
            };
            let editing = form.editing_target.is_some();
            let read_only = form.read_only;
            ui.horizontal(|ui| {
                ui.heading(if read_only { "Requirement" } else if editing { "Edit Requirement" } else { "New Requirement" });
                // Only an already-existing entry's viewer has anything to
                // switch into editing — a create-mode form is already
                // editable, nothing to toggle.
                if read_only {
                    if ui.button("Edit").clicked() {
                        edit_clicked = true;
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
                    if ui.add_enabled(!busy, egui::Button::new(button_label)).clicked() {
                        create_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Name:");
                if read_only {
                    ui.label(&form.name);
                } else {
                    // Renaming isn't supported — an edit's target
                    // LogicalPath is fixed, so the name field is
                    // display-only once open in edit mode (see
                    // forms.rs's build_command).
                    if ui.add_enabled(!editing, egui::TextEdit::singleline(&mut form.name)).changed() {
                        form.edited = true;
                    }
                }
            });
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
                ui.label("Requirement text:");
                if ui.text_edit_multiline(&mut form.requirement_text).changed() {
                    form.edited = true;
                }
                ui.label("Requirement guidance:");
                if ui.text_edit_multiline(&mut form.requirement_guidance).changed() {
                    form.edited = true;
                }
                ui.label("Test guidance:");
                if ui.text_edit_multiline(&mut form.test_guidance).changed() {
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
            ui.label("Dependencies:");
            let mut remove_dependency: Option<usize> = None;
            let mut dependency_edited = false;
            for (i, dep) in form.dependencies.iter_mut().enumerate() {
                if read_only {
                    ui.label(dep.to_string());
                } else {
                    ui.horizontal(|ui| {
                        dependency_edited |= render_dependency_kind_picker(ui, dep);
                        if ui.button("Remove").clicked() {
                            remove_dependency = Some(i);
                        }
                    });
                    dependency_edited |= render_dependency_fields(ui, dep);
                }
            }
            if let Some(i) = remove_dependency {
                form.dependencies.remove(i);
                dependency_edited = true;
            }
            if dependency_edited {
                form.edited = true;
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
                render_dependency_fields(ui, &mut form.new_dependency);
                if ui.button("Add dependency").clicked() {
                    form.dependencies.push(form.new_dependency.clone());
                    form.new_dependency = DependencyDraft::default();
                    form.edited = true;
                }
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
        }
        if edit_clicked {
            self.editor_edit_clicked();
        } else if create_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
        } else if add_attachment_clicked {
            self.local_attachment_add_clicked(LocalPoolKind::RequirementAttachment);
        } else if let Some(path) = remove_attachment {
            self.local_attachment_remove_clicked(LocalPoolKind::RequirementAttachment, path);
        }
    }

    fn render_test_form(&mut self, ui: &mut egui::Ui) {
        let mut create_clicked = false;
        let mut cancel_clicked = false;
        let mut edit_clicked = false;
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        let mut add_template_clicked = false;
        let mut remove_template: Option<PathBuf> = None;
        {
            let EditorState::NewTest(form) = &mut self.editor else {
                return;
            };
            let editing = form.editing_target.is_some();
            let read_only = form.read_only;
            ui.horizontal(|ui| {
                ui.heading(if read_only { "Test" } else if editing { "Edit Test" } else { "New Test" });
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
                    if ui.add_enabled(!busy, egui::Button::new(button_label)).clicked() {
                        create_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Name:");
                if read_only {
                    ui.label(&form.name);
                } else if ui.add_enabled(!editing, egui::TextEdit::singleline(&mut form.name)).changed() {
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
                        .radio(matches!(form.result_kind, ResultKindV1::FreeForm), "Free Form")
                        .clicked()
                    {
                        form.result_kind = ResultKindV1::FreeForm;
                        form.edited = true;
                    }
                    if ui
                        .radio(matches!(form.result_kind, ResultKindV1::Template), "Template")
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
        }
        if edit_clicked {
            self.editor_edit_clicked();
        } else if create_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
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
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        {
            let EditorState::NewResult(form) = &mut self.editor else {
                return;
            };
            let editing = form.editing_target.is_some();
            let read_only = form.read_only;
            ui.horizontal(|ui| {
                ui.heading(if read_only { "Result" } else if editing { "Edit Result" } else { "New Result" });
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
                    if ui.add_enabled(!busy, egui::Button::new(button_label)).clicked() {
                        create_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Name:");
                if read_only {
                    ui.label(&form.name);
                } else if ui.add_enabled(!editing, egui::TextEdit::singleline(&mut form.name)).changed() {
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
                    if ui.text_edit_singleline(&mut form.requirement_path).changed() {
                        form.edited = true;
                    }
                    // A picker, not a replacement for the field above —
                    // typing the path by hand still works (e.g. pasting
                    // one, or for when the target hasn't loaded into
                    // `self.tree` yet). Selecting an entry here just
                    // fills the same field with the correctly-formatted
                    // absolute reference path, so the user doesn't have
                    // to know `logical`'s
                    // `/[modules/<sub>/]*requirements/<name>` syntax by
                    // heart.
                    if let Some(tree) = &self.tree {
                        egui::ComboBox::from_id_salt("pick_requirement_path")
                            .selected_text("Pick…")
                            .show_ui(ui, |ui| {
                                for target in flatten_leaf_paths(tree, EntryKind::Requirement) {
                                    let path_str = absolute_reference_path(&target, "requirements");
                                    let selected = form.requirement_path == path_str;
                                    if ui.selectable_label(selected, target.to_string()).clicked() {
                                        form.requirement_path = path_str;
                                        form.edited = true;
                                    }
                                }
                            });
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Requirement commit:");
                    if ui.text_edit_singleline(&mut form.requirement_commit).changed() {
                        form.edited = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Test path:");
                    if ui.text_edit_singleline(&mut form.test_path).changed() {
                        form.edited = true;
                    }
                    if let Some(tree) = &self.tree {
                        egui::ComboBox::from_id_salt("pick_test_path")
                            .selected_text("Pick…")
                            .show_ui(ui, |ui| {
                                for target in flatten_leaf_paths(tree, EntryKind::Test) {
                                    let path_str = absolute_reference_path(&target, "tests");
                                    let selected = form.test_path == path_str;
                                    if ui.selectable_label(selected, target.to_string()).clicked() {
                                        form.test_path = path_str;
                                        form.edited = true;
                                    }
                                }
                            });
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
        }
        if edit_clicked {
            self.editor_edit_clicked();
        } else if create_clicked {
            self.editor_create_clicked();
        } else if cancel_clicked {
            self.editor_cancel_clicked();
        } else if add_attachment_clicked {
            self.local_attachment_add_clicked(LocalPoolKind::ResultAttachment);
        } else if let Some(path) = remove_attachment {
            self.local_attachment_remove_clicked(LocalPoolKind::ResultAttachment, path);
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
                if ui.add_enabled(!creating, egui::Button::new("Create")).clicked() {
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

    pub(crate) fn render_rename_module_dialog(&mut self, ui: &mut egui::Ui) {
        if self.rename_module_dialog.is_none() {
            return;
        }

        let mut confirm_clicked = false;
        let mut cancel_clicked = false;
        egui::Modal::new(egui::Id::new("rename_module_dialog")).show(ui.ctx(), |ui| {
            let Some(dialog) = &mut self.rename_module_dialog else {
                return;
            };
            ui.heading("Rename Module");
            let path_label = dialog
                .target
                .iter()
                .map(EntryName::as_str)
                .collect::<Vec<_>>()
                .join("/");
            ui.label(format!("Renaming: {path_label}"));
            ui.horizontal(|ui| {
                ui.label("New name:");
                ui.text_edit_singleline(&mut dialog.new_name);
            });
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            let busy = dialog.pending_request.is_some();
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Rename")).clicked() {
                    confirm_clicked = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
            });
        });

        if confirm_clicked {
            self.rename_module_dialog_confirmed();
        } else if cancel_clicked {
            self.rename_module_dialog_cancelled();
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

    #[cfg(all(feature = "debug-panel", debug_assertions))]
    pub(crate) fn render_debug_panel(&mut self, ui: &mut egui::Ui) {
        if !self.debug.open {
            return;
        }

        egui::Panel::right("debug_panel").default_size(320.0).size_range(240.0..=600.0).show(ui, |ui| {
            ui.heading("Debug");

            ui.separator();
            ui.label("Local gui-ui state:");
            ui.label(format!("pending: {}", self.pending.len()));
            ui.label(format!("dirty: {}", self.dirty));
            ui.label(format!("selection: {:?}", self.selection));
            ui.label(format!("selected_module: {:?}", self.selected_module));
            ui.label(format!("project_path: {:?}", self.project_path));
            ui.label(format!("nav_history: {} entries, position {}", self.nav_history.len(), self.nav_position));
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
                ui.colored_label(egui::Color32::YELLOW, "Tx is currently stalled — commands are queuing.");
            }
            if self.debug.is_rx_stalled(std::time::Instant::now()) {
                ui.colored_label(egui::Color32::YELLOW, "Rx is currently stalled — events are queuing.");
            }
            // No "Rx Failure" button — a genuine one (an `Event` `gui-core`
            // computed but never sent) needs real `gui-core` cooperation
            // to reproduce honestly, which isn't built yet; see README's
            // "Planned: debug side panel" for the open decision on
            // whether that's worth adding to `gui-core`'s production
            // `Command` enum for a purely diagnostic feature.
            ui.label("(Rx Failure not implemented — see README)");

            ui.separator();
            ui.label(format!("Message log ({} entries, oldest first):", self.debug.log.len()));
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
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
                    ui.label(format!("[{:>6.1}s] {prefix}: {}", entry.at.elapsed().as_secs_f32(), entry.detail));
                }
            });
        });
    }
}

fn render_tree_node(app: &mut GuiApp, ui: &mut egui::Ui, node: &TreeNode, module_path: &[EntryName]) {
    match node.kind {
        EntryKind::Module => {
            // A module with no matching descendant (filter active, no
            // leaf under it — at any depth — has a fully-qualified path
            // containing the filter text) is skipped entirely, not just
            // collapsed — see `node_matches_filter`'s own doc comment.
            if !node_matches_filter(node, module_path, &app.tree_filter) {
                return;
            }

            let mut this_module_path = module_path.to_vec();
            this_module_path.push(node.name.clone());
            let is_current = app.selected_module == this_module_path;

            ui.horizontal(|ui| {
                // A module has no `EntryDetail`/form of its own (see that
                // type's doc comment) — this button is the only way to
                // make it the "current module" new entries and the
                // Attachments dialog target; the CollapsingHeader label
                // itself only toggles expand/collapse.
                let glyph = if is_current { "◉" } else { "○" };
                if ui.small_button(glyph).on_hover_text("Set as current module").clicked() {
                    if app.editor_has_unsaved_edits() {
                        app.unsaved_form_dialog_opened(PendingNavigation::SelectModule(this_module_path.clone()));
                    } else {
                        app.select_module(this_module_path.clone());
                    }
                }
                if ui.small_button("✎").on_hover_text("Rename this module").clicked() {
                    app.rename_module_dialog_opened(this_module_path.clone());
                }
                egui::CollapsingHeader::new(node.name.as_str())
                    .default_open(module_path.is_empty())
                    .show(ui, |ui| {
                        render_module_children(app, ui, &node.children, &this_module_path);
                    });
            });
        }
        // Only reached via `render_leaf_group` below — a module's
        // children are grouped by kind before being rendered, never
        // walked one-by-one in raw `TreeNode` order.
        EntryKind::Requirement | EntryKind::Test | EntryKind::Result => {
            render_leaf(app, ui, node, module_path);
        }
    }
}

/// Renders one module's children: submodules directly (each recursing
/// back into `render_tree_node`), then requirement/test/result leaves
/// grouped under three collapsible "requirements"/"tests"/"results"
/// folders — a real on-disk grouping (`disk`'s own project layout: each
/// module has separate `requirements/`, `tests/`, `results/`
/// directories), not just a display convenience, so mirroring it here
/// keeps the tree's shape legible instead of interleaving three
/// unrelated kinds of leaf in whatever order `ModuleDraft`'s maps happen
/// to iterate.
fn render_module_children(app: &mut GuiApp, ui: &mut egui::Ui, children: &[TreeNode], module_path: &[EntryName]) {
    for child in children {
        if child.kind == EntryKind::Module {
            render_tree_node(app, ui, child, module_path);
        }
    }
    render_leaf_group(app, ui, "requirements", EntryKind::Requirement, children, module_path);
    render_leaf_group(app, ui, "tests", EntryKind::Test, children, module_path);
    render_leaf_group(app, ui, "results", EntryKind::Result, children, module_path);
}

/// One collapsible folder ("requirements"/"tests"/"results") holding
/// every child of `kind` — omitted entirely when there are none, so an
/// empty module doesn't grow three empty, useless folders.
fn render_leaf_group(
    app: &mut GuiApp,
    ui: &mut egui::Ui,
    title: &str,
    kind: EntryKind,
    children: &[TreeNode],
    module_path: &[EntryName],
) {
    let matching: Vec<&TreeNode> = children
        .iter()
        .filter(|child| child.kind == kind && node_matches_filter(child, module_path, &app.tree_filter))
        .collect();
    if matching.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(title).default_open(true).show(ui, |ui| {
        for leaf in matching {
            render_leaf(app, ui, leaf, module_path);
        }
    });
}

fn render_leaf(app: &mut GuiApp, ui: &mut egui::Ui, node: &TreeNode, module_path: &[EntryName]) {
    let label = match node.kind {
        EntryKind::Requirement => format!("{} {}", status_glyph(node.status), node.name.as_str()),
        _ => node.name.as_str().to_string(),
    };
    if ui.selectable_label(false, label).clicked() {
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

fn status_glyph(status: EntryStatus) -> &'static str {
    match status {
        EntryStatus::Met => "✓",
        EntryStatus::Unmet => "✗",
        EntryStatus::Unvalidated => "•",
    }
}

/// Every requirement (or test) in the currently-loaded tree, as
/// `LogicalPath`s — the Result form's path pickers' option list. Walks
/// `tree.root.children` directly (not `render_tree_node`'s recursion),
/// for the same reason `render_left_pane` renders the root specially: the
/// root `TreeNode`'s own `name` is a display label, not a real
/// module-path segment, and must never be pushed into a child's path.
fn flatten_leaf_paths(tree: &TreeSnapshot, kind: EntryKind) -> Vec<LogicalPath> {
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
    if ui.radio(matches!(dep, DependencyDraft::LocalRequirement { .. }), "Local").clicked() {
        *dep = DependencyDraft::LocalRequirement {
            path: String::new(),
            commit: String::new(),
        };
        return true;
    }
    if ui.radio(matches!(dep, DependencyDraft::Remote { .. }), "Remote").clicked() {
        *dep = DependencyDraft::Remote {
            url: String::new(),
            path: String::new(),
            commit: String::new(),
        };
        return true;
    }
    if ui.radio(matches!(dep, DependencyDraft::Submodules), "Submodules").clicked() {
        *dep = DependencyDraft::Submodules;
        return true;
    }
    false
}

/// `dep`'s own editable fields, per variant — `Submodules` has none.
/// Shared the same way `render_dependency_kind_picker` is, including its
/// return-value convention.
fn render_dependency_fields(ui: &mut egui::Ui, dep: &mut DependencyDraft) -> bool {
    match dep {
        DependencyDraft::LocalRequirement { path, commit } => {
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label("Path:");
                changed |= ui.text_edit_singleline(path).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Commit:");
                changed |= ui.text_edit_singleline(commit).changed();
            });
            changed
        }
        DependencyDraft::Remote { url, path, commit } => {
            let mut changed = false;
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
            });
            changed
        }
        DependencyDraft::Submodules => false,
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
fn absolute_reference_path(target: &LogicalPath, kind_segment: &str) -> String {
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
/// leaf path of its own, so no sensible mapping — `node_matches_filter`
/// is the only caller, and it never reaches this arm for a `Module`
/// (that case is handled directly, recursing into children instead).
fn leaf_kind_segment(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Requirement => "requirements",
        EntryKind::Test => "tests",
        EntryKind::Result => "results",
        EntryKind::Module => unreachable!("a module has no leaf path of its own"),
    }
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
            node.children.iter().any(|child| node_matches_filter(child, &this_module_path, &filter))
        }
        leaf_kind => {
            let target = LogicalPath {
                modules: module_path.to_vec(),
                name: node.name.clone(),
            };
            absolute_reference_path(&target, leaf_kind_segment(leaf_kind)).to_lowercase().contains(&filter)
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
        assert_eq!(absolute_reference_path(&target, "requirements"), "/requirements/definition");
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
                vec![leaf(EntryKind::Requirement, "definition"), leaf(EntryKind::Test, "generic_test")],
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
                vec![module("setup", vec![leaf(EntryKind::Requirement, "nested_requirement")])],
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
                vec![leaf(EntryKind::Requirement, "definition"), leaf(EntryKind::Result, "definition")],
            ),
            can_undo: false,
            can_redo: false,
        };

        assert_eq!(flatten_leaf_paths(&tree, EntryKind::Test), Vec::new());
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
        let tree = module("setup", vec![module("nested", vec![leaf(EntryKind::Test, "generic_test")])]);
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
