//! Rendering only — every decision (what a click *means*) lives in
//! `lib.rs`'s plain methods (`on_exit_clicked`, `select`, ...); this file
//! just draws widgets and calls them. See README's "Logic: keep it out of
//! `update()`" and "Layout".

use std::collections::BTreeSet;
use std::path::PathBuf;

use gui_core::{
    EntryKind, EntryName, EntryStatus, LogicalPath, ReferenceAction, ReferencePath,
    ReferenceSiteKind, RequirementMetStatus, ResultKindV1, TestUnmetReason, TreeNode, TreeSnapshot,
    UnmetReason, title_case_from_name,
};

use crate::spellcheck::{
    FieldSpellCache, SpellChecker, SpellField, SpellPopupState, SuggestionRequest, SuggestionState,
    range_is_valid_for, replace_word_in_place, word_at_byte_offset,
};
use crate::{
    AutoCommitKind, DependencyDraft, DependencySlot, EditorState, ExitDialogState, GuiApp,
    LeafKind, LocalPoolKind, PathPickerScope, PathPickerTarget, PendingNavigation,
    PendingProjectAction, PushDialogState, TestRefDraft, TestRefSlot, ThemeChoice,
    ValidateBeforeSaveDialogState, absolute_reference_path, absolute_result_path,
    default_result_name, direct_submodule_names, flatten_leaf_paths, icons, leaf_kind_segment,
    scoped_leaf_paths, theme_colors, today_iso_date,
};

/// A short label for a `ReferenceSiteKind`, for the broken-references
/// modal's rows — see `GuiApp::render_broken_references_dialog`.
fn reference_site_kind_label(kind: &ReferenceSiteKind) -> &'static str {
    match kind {
        ReferenceSiteKind::RequirementTestReference { .. } => "test procedure",
        ReferenceSiteKind::RequirementDependency { .. } => "dependency",
        ReferenceSiteKind::RequirementSubmoduleDependency { .. } => "submodule dependency",
        ReferenceSiteKind::ResultTestRef { .. } => "result's test procedure",
    }
}

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
/// A read-only text/guidance field: its label is always drawn, but the
/// `Frame::group` around the content is only drawn when there's content to
/// set off — an empty field would otherwise show as a frame around nothing.
fn render_grouped_text(ui: &mut egui::Ui, label: &str, text: &str) {
    ui.label(label);
    if text.is_empty() {
        ui.label(text);
    } else {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(text);
        });
    }
}

/// Vertical breathing room between the requirement viewer's Requirement
/// guidance/Dependencies/Test procedures/Results sections, sized to match
/// `ui.separator()`'s own footprint (its default 6.0 line-height plus the
/// `item_spacing` it would otherwise pick up on either side) but without
/// painting the line itself — those sections are visually set off by their
/// own `Frame::group` borders already, so the line was redundant.
fn section_gap(ui: &mut egui::Ui) {
    ui.add_space(ui.spacing().item_spacing.y + 6.0);
}

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
        UnmetReason::NoTests => vec!["It has no test procedures.".to_string()],
        UnmetReason::NotYetSaved => {
            vec!["It hasn't been saved yet, so there's no known commit to check.".to_string()]
        }
        UnmetReason::UnsatisfiedTests(tests) => tests
            .iter()
            .map(|unsatisfied| {
                let why = match unsatisfied.reason {
                    TestUnmetReason::UnresolvedReference => {
                        "its reference doesn't resolve to a real test procedure"
                    }
                    TestUnmetReason::TestNotYetSaved => "the test procedure hasn't been saved yet",
                    TestUnmetReason::StaleReference => {
                        "its reference is stale (pointing at an old commit of the test procedure)"
                    }
                    TestUnmetReason::NoPassingResult => "no current, passing result exists for it",
                };
                format!("Test procedure \"{}\": {why}.", unsatisfied.test)
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
/// Everything a spellchecked field needs, bundled so `spellchecked_singleline`/
/// `resizable_multiline` don't need a 5-6-parameter signature each. Built
/// fresh at each of the 7 in-scope call sites from that field's own slice
/// of its form (`&self.spell_checker`, `&self.config.spellcheck_custom_words`,
/// and the form's own `<field>_spell`/`spell_popup`).
struct SpellCtx<'a> {
    checker: &'a SpellChecker,
    /// `self.config.spellcheck_enabled` — the status bar's checkbox.
    /// Checked alongside `checker.is_ready()` everywhere that matters, so
    /// disabling it behaves exactly like the dictionary never having
    /// finished building: no layouter, no popup.
    enabled: bool,
    custom_words: &'a BTreeSet<String>,
    cache: &'a mut FieldSpellCache,
    popup: &'a mut Option<SpellPopupState>,
    field: SpellField,
}

/// What happened inside a field's right-click suggestion popup this frame
/// — returned by `attach_spellcheck_popup` so its two callers
/// (`spellchecked_singleline`/`resizable_multiline`'s spellchecked branch)
/// can apply it without duplicating the match themselves (see
/// `apply_spell_popup_action`).
enum SpellPopupAction {
    /// A suggestion was clicked — the word has already been replaced in
    /// the buffer (that's the one action here that must happen inside
    /// `attach_spellcheck_popup` itself, since it's the only thing with
    /// both the click and `text` in scope at the same time).
    Replace,
    /// "Add to dictionary" was clicked, carrying the word to add — left
    /// for the caller to actually insert into `GuiConfig` and save, since
    /// this function never sees `GuiConfig` at all (same "free function
    /// inside `&mut self.editor`'s borrow can't call back into `self`"
    /// reasoning as every other deferred-click flag in this file).
    AddToDictionary(String),
}

/// Applies a `SpellPopupAction` returned by `attach_spellcheck_popup`:
/// folds `Replace` into the caller's own `changed` flag (the widget's own
/// `Response::changed()`, computed during `.show()`, can't reflect a
/// mutation `attach_spellcheck_popup` makes afterward) and passes
/// `AddToDictionary`'s word up one more level, to wherever `GuiConfig` is
/// actually reachable.
fn apply_spell_popup_action(
    action: Option<SpellPopupAction>,
    changed: &mut bool,
) -> Option<String> {
    match action {
        Some(SpellPopupAction::Replace) => {
            *changed = true;
            None
        }
        Some(SpellPopupAction::AddToDictionary(word)) => Some(word),
        None => None,
    }
}

/// Builds a `TextEdit::layouter` that refreshes `cache` against the exact
/// buffer content it's about to lay out, then underlines whatever
/// misspellings that refresh finds — used by both `spellchecked_singleline`
/// and `resizable_multiline`. `refresh` is a cheap no-op unless the text
/// actually changed (see its own doc comment), so doing it here instead of
/// once before `.show()` is free — and it's the only way to guarantee the
/// ranges sliced below always match the string being sliced: `TextEdit::
/// show` applies this frame's keystroke to the buffer *before* invoking the
/// layouter, so a `cache.refresh` call made before `.show()` (the previous
/// approach) can scan stale, pre-edit text while this closure — and
/// everything downstream of it — sees the post-edit buffer, letting a
/// shrinking edit produce misspelling ranges that run past the new, shorter
/// string. Matches the default layouter's own font/color choice (see
/// `egui::TextEdit::show`'s internal `default_layouter`) so a field with no
/// misspellings looks identical to before this feature existed.
fn spellcheck_text_layouter<'a>(
    cache: &'a mut FieldSpellCache,
    checker: &'a SpellChecker,
    custom_words: &'a BTreeSet<String>,
    break_on_newline: bool,
) -> impl FnMut(&egui::Ui, &dyn egui::TextBuffer, f32) -> std::sync::Arc<egui::Galley> + 'a {
    move |ui, buffer, wrap_width| {
        let text = buffer.as_str();
        cache.refresh(text, checker, custom_words);
        let text_color = ui
            .visuals()
            .override_text_color
            .unwrap_or_else(|| ui.visuals().widgets.inactive.text_color());
        let font_id = egui::FontSelection::default().resolve(ui.style());
        let underline_color = theme_colors::misspelling_underline_color(ui.visuals().dark_mode);

        let normal_format = egui::TextFormat::simple(font_id, text_color);
        let mut misspelled_format = normal_format.clone();
        misspelled_format.underline = egui::Stroke::new(1.5, underline_color);

        let mut job = egui::text::LayoutJob {
            break_on_newline,
            wrap: egui::text::TextWrapping {
                max_width: wrap_width,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut cursor = 0;
        for m in cache.misspellings() {
            if m.range.start > cursor {
                job.append(&text[cursor..m.range.start], 0.0, normal_format.clone());
            }
            job.append(&text[m.range.clone()], 0.0, misspelled_format.clone());
            cursor = m.range.end;
        }
        if cursor < text.len() || job.sections.is_empty() {
            job.append(&text[cursor..], 0.0, normal_format);
        }

        ui.fonts_mut(|f| f.layout_job(job))
    }
}

/// The right-click side of a spellchecked field: opens/updates `popup` on
/// a secondary click over a misspelled word (spawning a background
/// suggestion lookup), and renders the popup's contents from whatever
/// `popup` currently holds. Called after `.show()`, so — unlike the
/// layouter above — the click detection here always reflects last frame's
/// misspelling ranges, one frame behind a keystroke that just changed
/// them; self-corrects the next frame, same as the layouter's own
/// one-frame lag against a same-frame edit.
fn attach_spellcheck_popup(
    output: &egui::text_edit::TextEditOutput,
    text: &mut String,
    checker: &SpellChecker,
    cache: &mut FieldSpellCache,
    popup: &mut Option<SpellPopupState>,
    field: SpellField,
) -> Option<SpellPopupAction> {
    if output.response.secondary_clicked() {
        if let Some(pos) = output.response.interact_pointer_pos() {
            let local = pos - output.galley_pos;
            let char_idx = output.galley.cursor_from_pos(local).index;
            let byte_offset = text
                .char_indices()
                .nth(char_idx.0)
                .map(|(offset, _)| offset)
                .unwrap_or(text.len());
            if let Some(m) = word_at_byte_offset(cache.misspellings(), byte_offset) {
                if let Some(request) = SuggestionRequest::spawn(checker, m.word.clone()) {
                    *popup = Some(SpellPopupState::new(field, m.clone(), request));
                }
            }
        }
    }

    let mut action = None;
    output.response.context_menu(|ui| {
        let Some(state) = popup.as_ref().filter(|state| state.field == field) else {
            ui.close();
            return;
        };
        ui.label(format!("\u{201c}{}\u{201d}", state.misspelling.word));
        ui.separator();
        match &state.suggestions {
            SuggestionState::Loading(_) => {
                ui.add_enabled(false, egui::Label::new("Loading suggestions…"));
            }
            SuggestionState::Ready(list) if list.is_empty() => {
                ui.add_enabled(false, egui::Label::new("No suggestions"));
            }
            SuggestionState::Ready(list) => {
                for suggestion in list {
                    if ui.button(suggestion).clicked() {
                        // `state.misspelling.range` was captured whenever
                        // the popup opened and the field may have been
                        // edited since — re-validate against the buffer's
                        // current length/char boundaries rather than
                        // trusting a range that's gone stale, since both
                        // indexing and `replace_range` panic otherwise.
                        if range_is_valid_for(text, &state.misspelling.range) {
                            replace_word_in_place(text, &state.misspelling.range, suggestion);
                            action = Some(SpellPopupAction::Replace);
                        }
                        ui.close();
                    }
                }
            }
        }
        ui.separator();
        if ui.button("Add to dictionary").clicked() {
            action = Some(SpellPopupAction::AddToDictionary(
                state.misspelling.word.clone(),
            ));
            ui.close();
        }
        if ui.button("Ignore").clicked() {
            ui.close();
        }
    });

    if action.is_some() {
        cache.invalidate();
        *popup = None;
    }
    action
}

/// A single-line spellchecked prose field (a requirement/test/result
/// title) — the singleline counterpart to `resizable_multiline`, and the
/// first shared singleline wrapper in this file (every other singleline
/// field calls `text_edit_singleline`/`TextEdit::singleline` directly).
/// Returns `(changed, word added to dictionary)` rather than a bare
/// `Response`, since call sites only ever checked `.changed()` and this
/// additionally needs to surface an "Add to dictionary" click up to
/// wherever `GuiConfig` is reachable — see `SpellPopupAction`'s own doc
/// comment on why that can't just happen in here.
fn spellchecked_singleline(
    ui: &mut egui::Ui,
    text: &mut String,
    spell: SpellCtx<'_>,
) -> (bool, Option<String>) {
    if !spell.checker.is_ready() || !spell.enabled {
        // Behaves exactly as before this feature existed — no layouter,
        // no popup — while the dictionary is still building or failed to
        // build at all, or the user has switched spellcheck off.
        return (ui.text_edit_singleline(text).changed(), None);
    }
    let SpellCtx {
        checker,
        custom_words,
        cache,
        popup,
        field,
        ..
    } = spell;
    let mut layouter = spellcheck_text_layouter(cache, checker, custom_words, false);
    let output = egui::TextEdit::singleline(text)
        .layouter(&mut layouter)
        .show(ui);
    // Ends the layouter's own borrow of `cache` explicitly — otherwise it
    // (an `impl Trait` closure) is conservatively considered to borrow
    // `cache` until this scope ends, even though nothing calls it again
    // after `.show()` above.
    drop(layouter);
    let mut changed = output.response.changed();
    let action = attach_spellcheck_popup(&output, text, checker, cache, popup, field);
    let added_to_dictionary = apply_spell_popup_action(action, &mut changed);
    (changed, added_to_dictionary)
}

/// See `spellchecked_singleline`'s own doc comment on the return type —
/// same reasoning here.
fn resizable_multiline(
    ui: &mut egui::Ui,
    id_salt: &str,
    text: &mut String,
    spell: SpellCtx<'_>,
) -> (bool, Option<String>) {
    resizable_multiline_with_max_height(ui, id_salt, text, f32::INFINITY, Some(spell))
}

/// Same as `resizable_multiline`, but caps how tall the user can drag the
/// box — for callers (like the commit-all dialog) that aren't inside their
/// own scroll area and would otherwise let a drag push the surrounding
/// modal/window past the screen's edge. `spell: None` (the commit-all
/// dialog's own call) renders exactly as before this feature existed.
fn resizable_multiline_with_max_height(
    ui: &mut egui::Ui,
    id_salt: &str,
    text: &mut String,
    max_height: f32,
    spell: Option<SpellCtx<'_>>,
) -> (bool, Option<String>) {
    let default_rows = text
        .lines()
        .count()
        .max(1)
        .clamp(MULTILINE_MIN_ROWS, MULTILINE_MAX_DEFAULT_ROWS);
    let available_width = ui.available_width();
    // Same "behaves exactly as before while not `Ready`/disabled"
    // reasoning as `spellchecked_singleline`.
    let spell = spell.filter(|spell| spell.checker.is_ready() && spell.enabled);
    let mut changed = false;
    let mut added_to_dictionary = None;
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
            // the frame without the box inside it following.
            let desired_size = ui.available_size();
            match spell {
                None => {
                    // Same "fill the frame" trick `add_sized` itself uses
                    // internally — kept spelled out here (rather than
                    // calling `add_sized`) so the spellchecked branch below
                    // can use the identical wrapping to also get at the
                    // full `TextEditOutput`, which `add_sized` discards.
                    changed = ui
                        .add_sized(desired_size, egui::TextEdit::multiline(text))
                        .changed();
                }
                Some(SpellCtx {
                    checker,
                    custom_words,
                    cache,
                    popup,
                    field,
                    ..
                }) => {
                    let mut layouter = spellcheck_text_layouter(cache, checker, custom_words, true);
                    let layout = egui::Layout::centered_and_justified(ui.layout().main_dir());
                    let output = ui
                        .allocate_ui_with_layout(desired_size, layout, |ui| {
                            egui::TextEdit::multiline(text)
                                .layouter(&mut layouter)
                                .show(ui)
                        })
                        .inner;
                    // See `spellchecked_singleline`'s own comment on why
                    // this is dropped explicitly.
                    drop(layouter);
                    changed = output.response.changed();
                    let action =
                        attach_spellcheck_popup(&output, text, checker, cache, popup, field);
                    added_to_dictionary = apply_spell_popup_action(action, &mut changed);
                }
            }
        });
    (changed, added_to_dictionary)
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

/// A module tree row's label: just its name, unless the project is
/// validated and the module's subtree (itself plus every nested submodule)
/// has at least one requirement, in which case its "requirements met"
/// percentage is appended. Suppressed for an unvalidated project (nothing
/// meaningful to report yet — see `TreeSnapshot::validated`'s own doc
/// comment) and for a subtree with no requirements at all (a bare "(0%)"
/// would read as "unmet" rather than "nothing to measure").
fn module_label(node: &TreeNode, project_validated: bool) -> String {
    if project_validated && node.requirement_count > 0 {
        let met_pct = percentage(node.requirements_met, node.requirement_count);
        format!("{} ({met_pct:.0}%)", node.name.as_str())
    } else {
        node.name.as_str().to_string()
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
                if icon_button(ui, true, icons::VALIDATE, "Validate").clicked() {
                    self.validate_clicked();
                }
                ui.separator();
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
                if icon_button(ui, self.tree.is_some(), icons::PUSH, "Push…").clicked() {
                    self.push_button_clicked();
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
                if icon_button(ui, true, icons::NEW_TEST, "New Test Procedure").clicked() {
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

                // "Clear", pinned to the toolbar's far right corner — same
                // `right_to_left` sub-layout technique the menu bar's own
                // debug-panel toggle and the status bar's zoom controls use.
                // Pushed into `nav_history` as a `NavTarget::Empty` (see
                // `clear_center_pane_clicked`) rather than just blanking
                // `self.editor`/`self.selection` directly, so Back can
                // still return to whatever was showing before the click —
                // same "every navigation is Back-able" convention Back/
                // Forward and the four "New ___" buttons already follow.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, self.can_clear_center_pane(), icons::CLEAR, "Clear")
                        .clicked()
                    {
                        if self.editor_has_unsaved_edits() {
                            self.unsaved_form_dialog_opened(PendingNavigation::Clear);
                        } else {
                            self.clear_center_pane_clicked();
                        }
                    }
                    // "Refresh", immediately left of "Clear" (this
                    // `right_to_left` layout renders right-to-left, so this
                    // call lands just to Clear's left) — re-fetches the
                    // currently open view's cached data in place. Doesn't
                    // touch `editor_has_unsaved_edits`/`PendingNavigation`
                    // the way Back/Forward/Clear/the "New ___" buttons do:
                    // it never replaces or clears `self.editor`, so there's
                    // nothing here that could clobber an in-progress edit.
                    if icon_button(ui, self.tree.is_some(), icons::REFRESH, "Refresh").clicked() {
                        self.refresh_clicked();
                    }
                });
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

                    let mut spellcheck_enabled = self.config.spellcheck_enabled;
                    if ui.checkbox(&mut spellcheck_enabled, "Spellcheck").changed() {
                        self.spellcheck_toggled(spellcheck_enabled);
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

                        // A resizable nested `Panel::top`, not a fixed
                        // 0.4/0.6 height split — its drag handle lets the
                        // user trade space between the module tree above
                        // and the selected-module detail below, and (like
                        // the outer `tree_pane` itself) egui persists the
                        // dragged height across frames/restarts keyed by
                        // this panel's id, so no manual state is needed
                        // here.
                        egui::Panel::top("tree_pane_modules_panel")
                            .resizable(true)
                            .default_size(ui.available_height() * 0.6)
                            .size_range(60.0..=f32::INFINITY)
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("tree_pane_modules")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        // The root `TreeNode`'s own `name` is
                                        // the project's display name (see
                                        // `gui-core`'s `build_tree_snapshot`)
                                        // — a label, not a real module-path
                                        // segment. Unlike every other
                                        // `Module` node, it must never be
                                        // pushed into a path, so the root is
                                        // rendered specially here (its own
                                        // selector + iterating its children
                                        // with an *empty* path) rather than
                                        // through `render_tree_node`, which
                                        // pushes `node.name` for every module
                                        // it handles.
                                        let is_root_current = self.selected_module.is_empty();
                                        ui.horizontal(|ui| {
                                            let glyph = if is_root_current {
                                                icons::MODULE_CURRENT
                                            } else {
                                                icons::MODULE_NOT_CURRENT
                                            };
                                            let mut text = egui::RichText::new(glyph);
                                            if is_root_current {
                                                text =
                                                    text.color(theme_colors::module_current_color(
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
                                            let mut root_text = egui::RichText::new(module_label(
                                                &root,
                                                tree.validated,
                                            ))
                                            .strong();
                                            if is_root_current {
                                                root_text = root_text.color(
                                                    theme_colors::module_current_color(
                                                        ui.visuals().dark_mode,
                                                    ),
                                                );
                                            }
                                            // `Sense::click()` — see the
                                            // no-submodules branch of
                                            // `render_tree_node` for why a
                                            // plain `ui.label` can't host a
                                            // context menu.
                                            let root_response = ui.add(
                                                egui::Label::new(root_text)
                                                    .sense(egui::Sense::click()),
                                            );
                                            attach_paste_requirement_menu(
                                                &root_response,
                                                self,
                                                Vec::new(),
                                            );
                                        });
                                        render_module_children(
                                            self,
                                            ui,
                                            &root.children,
                                            &[],
                                            force_open,
                                            tree.validated,
                                        );
                                    });
                            });

                        ui.separator();

                        egui::ScrollArea::vertical()
                            .id_salt("tree_pane_selection")
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
                // `self.editor` being `None` here means nothing is
                // selected, a selection's `GetEntryDetail` reply hasn't
                // landed yet, or that reply came back empty (the entry was
                // deleted between being selected and the reply arriving —
                // most commonly hit via Back/Forward) — `select` clears
                // `editor` up front specifically so this branch can tell
                // "nothing to show yet" apart from "showing something," and
                // `selection_not_found` (set by `apply_entry_detail`) tells
                // the empty reply apart from one still in flight.
                Pane::Empty => match (&self.selection, self.selection_not_found) {
                    (None, _) => {
                        ui.label("Select an entry in the tree to view it, or use the toolbar to create a new one.");
                    }
                    (Some(_), true) => {
                        ui.label("This entry no longer exists.");
                    }
                    (Some(_), false) => {
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
        self.ensure_spell_checker_started();
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
        let mut create_result_clicked = false;
        // Set by a click on one of the read-only viewer's Dependencies/Test
        // references/Results links — acted on after `form`'s borrow of
        // `self.editor` ends below, same reasoning as every other
        // deferred-click flag in this function.
        let mut navigate_clicked: Option<gui_core::EntryPath> = None;
        // The same, for the one link here that points at a module rather
        // than a leaf entry — a `Submodule` dependency. Modules have no
        // `EntryPath` of their own (see that type's variants), so they go
        // through `select_module` instead of `select`.
        let mut navigate_module_clicked: Option<Vec<EntryName>> = None;
        // Set by any of this form's 4 prose fields' "Add to dictionary"
        // popup action — applied once, after `self.editor`'s borrow ends,
        // since `attach_spellcheck_popup` never sees `self.config` at all
        // (see `SpellPopupAction::AddToDictionary`'s own doc comment).
        let mut spellcheck_add_to_dictionary: Option<String> = None;
        // Set by the "Commit history" section the first time it's expanded
        // for an entry nothing's been fetched for yet — see
        // `render_commit_log_section`'s own doc comment on why this can't
        // just fire the fetch inline.
        let mut commit_log_expand_requested: Option<gui_core::EntryPath> = None;
        // Set by clicking a commit's hash link in the "Commit history"
        // section — see `render_commit_log_section`'s own doc comment.
        let mut commit_log_commit_clicked: Option<(gui_core::EntryPath, String)> = None;
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
                    ui.label("Identifier:");
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
                // Only a saved entry has a real on-disk path to show —
                // `read_only` (see `render_requirement_form`'s own module
                // doc comment) always implies `editing_target: Some(_)`,
                // so this and `read_only`'s own check are equivalent, but
                // spelled out explicitly here since it's what the path
                // itself is actually built from. `.selectable(true)` lets
                // the user drag-select and copy it directly, the point of
                // showing it at all.
                if let Some(target) = &form.editing_target {
                    ui.horizontal(|ui| {
                        ui.label("Path:");
                        ui.add(
                            egui::Label::new(absolute_reference_path(target, "requirements"))
                                .selectable(true),
                        );
                    });
                }
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
                    render_grouped_text(ui, "Requirement text:", &form.requirement_text);
                    render_grouped_text(ui, "Requirement guidance:", &form.requirement_guidance);
                    render_grouped_text(ui, "Test procedure guidance:", &form.test_guidance);
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        let (changed, added) = spellchecked_singleline(
                            ui,
                            &mut form.title,
                            SpellCtx {
                                checker: &self.spell_checker,
                                enabled: self.config.spellcheck_enabled,
                                custom_words: &self.config.spellcheck_custom_words,
                                cache: &mut form.title_spell,
                                popup: &mut form.spell_popup,
                                field: SpellField::Title,
                            },
                        );
                        if changed {
                            form.edited = true;
                        }
                        if spellcheck_add_to_dictionary.is_none() {
                            spellcheck_add_to_dictionary = added;
                        }
                        if ui
                            .button(icons::REGENERATE_TITLE)
                            .on_hover_text("Regenerate title from identifier")
                            .clicked()
                        {
                            form.title = title_case_from_name(&form.name);
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
                    let (changed, added) = resizable_multiline(
                        ui,
                        &format!("requirement_text:{entry_id}"),
                        &mut form.requirement_text,
                        SpellCtx {
                            checker: &self.spell_checker,
                            enabled: self.config.spellcheck_enabled,
                            custom_words: &self.config.spellcheck_custom_words,
                            cache: &mut form.requirement_text_spell,
                            popup: &mut form.spell_popup,
                            field: SpellField::RequirementText,
                        },
                    );
                    if changed {
                        form.edited = true;
                    }
                    if spellcheck_add_to_dictionary.is_none() {
                        spellcheck_add_to_dictionary = added;
                    }
                    ui.label("Requirement guidance:");
                    let (changed, added) = resizable_multiline(
                        ui,
                        &format!("requirement_guidance:{entry_id}"),
                        &mut form.requirement_guidance,
                        SpellCtx {
                            checker: &self.spell_checker,
                            enabled: self.config.spellcheck_enabled,
                            custom_words: &self.config.spellcheck_custom_words,
                            cache: &mut form.requirement_guidance_spell,
                            popup: &mut form.spell_popup,
                            field: SpellField::RequirementGuidance,
                        },
                    );
                    if changed {
                        form.edited = true;
                    }
                    if spellcheck_add_to_dictionary.is_none() {
                        spellcheck_add_to_dictionary = added;
                    }
                    ui.label("Test procedure guidance:");
                    let (changed, added) = resizable_multiline(
                        ui,
                        &format!("test_guidance:{entry_id}"),
                        &mut form.test_guidance,
                        SpellCtx {
                            checker: &self.spell_checker,
                            enabled: self.config.spellcheck_enabled,
                            custom_words: &self.config.spellcheck_custom_words,
                            cache: &mut form.test_guidance_spell,
                            popup: &mut form.spell_popup,
                            field: SpellField::TestGuidance,
                        },
                    );
                    if changed {
                        form.edited = true;
                    }
                    if spellcheck_add_to_dictionary.is_none() {
                        spellcheck_add_to_dictionary = added;
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
                section_gap(ui);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label("Dependencies:");
                    let mut remove_dependency: Option<usize> = None;
                    let mut dependency_edited = false;
                    for (i, dep) in form.dependencies.iter_mut().enumerate() {
                        if i > 0 {
                            ui.separator();
                        }
                        if read_only {
                            // `LocalRequirement` and `Submodule` are the two
                            // variants naming something else inside this same
                            // project, so both get a link to it, each behind a
                            // "Requirement:"/"Submodule:" label denoting the
                            // dependency's kind — `Remote` points outside it
                            // (nothing here to navigate to) and `Submodules`
                            // names no single entry at all, so those two stay
                            // plain labels.
                            match dep {
                                DependencyDraft::LocalRequirement { path, .. } => {
                                    let target = form.editing_target.as_ref().and_then(|t| {
                                        gui_core::resolve_reference_path(
                                            &ReferencePath(path.clone()),
                                            &t.modules,
                                            "requirements",
                                        )
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Requirement:");
                                        if let Some(target) = target {
                                            if ui.link(dep.brief()).clicked() {
                                                navigate_clicked =
                                                    Some(gui_core::EntryPath::Requirement(target));
                                            }
                                        } else {
                                            ui.label(dep.brief());
                                        }
                                    });
                                }
                                DependencyDraft::Submodule { name } => {
                                    // A submodule dependency names a *direct
                                    // child* module of this requirement's own
                                    // module by bare name, so there's no
                                    // reference path to parse — "does it
                                    // resolve" is a lookup in the tree instead,
                                    // and a name with no such child stays a
                                    // plain label (same treatment an
                                    // unparseable `LocalRequirement` path gets
                                    // above). Modules aren't `EntryPath`s, so
                                    // this navigates via `select_module` rather
                                    // than `navigate_clicked` — see
                                    // `navigate_module_clicked` below.
                                    let target = form.editing_target.as_ref().and_then(|t| {
                                        let tree = self.tree.as_ref()?;
                                        direct_submodule_names(tree, &t.modules)
                                            .iter()
                                            .any(|child| child == name)
                                            .then(|| {
                                                let mut module = t.modules.clone();
                                                module.push(EntryName(name.clone()));
                                                module
                                            })
                                    });
                                    // The "Submodule:" prefix stays a plain
                                    // label; the link itself shows the fully
                                    // qualified module path (this requirement's
                                    // own module plus the child's bare name),
                                    // not just the bare name, since several
                                    // sibling modules can share a child name.
                                    // Each level gets a `modules/` segment and
                                    // the whole thing a leading `/`, matching
                                    // the `modules/`-prefixed convention every
                                    // other path in this view uses (see
                                    // `LogicalPath::Display` and
                                    // `absolute_reference_path`) rather than
                                    // just joining bare module names.
                                    let full_path = form.editing_target.as_ref().map(|t| {
                                        let mut path = String::from("/");
                                        for module in t
                                            .modules
                                            .iter()
                                            .map(EntryName::as_str)
                                            .chain(std::iter::once(name.as_str()))
                                        {
                                            path.push_str("modules/");
                                            path.push_str(module);
                                            path.push('/');
                                        }
                                        path.pop();
                                        path
                                    });
                                    let label_text = full_path.unwrap_or_else(|| name.clone());
                                    ui.horizontal(|ui| {
                                        ui.label("Submodule:");
                                        match target {
                                            Some(module) => {
                                                if ui.link(label_text).clicked() {
                                                    navigate_module_clicked = Some(module);
                                                }
                                            }
                                            None => {
                                                ui.label(label_text);
                                            }
                                        }
                                    });
                                }
                                DependencyDraft::Remote { .. } | DependencyDraft::Submodules => {
                                    ui.label(dep.brief());
                                }
                            }
                        } else {
                            dependency_edited |= render_dependency_kind_dropdown(ui, i, dep);
                            let (changed, auto, pick_clicked) = render_dependency_fields(
                                ui,
                                dep,
                                self.tree.as_ref(),
                                &self.selected_module,
                            );
                            dependency_edited |= changed;
                            if let Some(kind) = auto {
                                auto_commit_clicked = Some((DependencySlot::Existing(i), kind));
                            }
                            if pick_clicked {
                                pick_dependency_path_clicked = Some(DependencySlot::Existing(i));
                            }
                            if ui.button("Remove").clicked() {
                                remove_dependency = Some(i);
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
                        if !form.dependencies.is_empty() {
                            ui.separator();
                        }
                        if form.adding_dependency {
                            egui::Modal::new(egui::Id::new("add_dependency_dialog")).show(
                                ui.ctx(),
                                |ui| {
                                    ui.heading("Add Dependency");
                                    ui.horizontal(|ui| {
                                        // Composing a not-yet-added entry isn't
                                        // itself an edit to the form's real
                                        // content — only actually clicking "Add
                                        // dependency" below is, so this return
                                        // value is deliberately ignored (unlike
                                        // the existing-row loop above).
                                        render_dependency_kind_picker(ui, &mut form.new_dependency);
                                    });
                                    let (_, auto, pick_clicked) = render_dependency_fields(
                                        ui,
                                        &mut form.new_dependency,
                                        self.tree.as_ref(),
                                        &self.selected_module,
                                    );
                                    if let Some(kind) = auto {
                                        auto_commit_clicked = Some((DependencySlot::New, kind));
                                    }
                                    if pick_clicked {
                                        pick_dependency_path_clicked = Some(DependencySlot::New);
                                    }
                                    ui.horizontal(|ui| {
                                        if ui.button("Add dependency").clicked() {
                                            form.dependencies.push(form.new_dependency.clone());
                                            form.new_dependency = DependencyDraft::default();
                                            form.adding_dependency = false;
                                            form.edited = true;
                                        }
                                        if ui.button("Cancel").clicked() {
                                            form.new_dependency = DependencyDraft::default();
                                            form.adding_dependency = false;
                                        }
                                    });
                                },
                            );
                        } else if ui.button("Add dependency").clicked() {
                            form.adding_dependency = true;
                        }
                    }
                });

                // Test references, like Dependencies above (and unlike Local
                // attachments below), aren't gated on `editing` — plain draft
                // data submitted whole on Save/Create, not a local file pool
                // requiring the entry to already exist.
                section_gap(ui);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label("Test procedures:");
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
                                    navigate_clicked = Some(gui_core::EntryPath::Test(target));
                                }
                            } else {
                                ui.label(test_ref.to_string());
                            }
                        } else {
                            let (changed, auto, pick_clicked) =
                                render_test_ref_fields(ui, test_ref, self.tree.as_ref());
                            test_ref_edited |= changed;
                            if let Some(target) = auto {
                                test_ref_auto_commit_clicked =
                                    Some((TestRefSlot::Existing(i), target));
                            }
                            if pick_clicked {
                                pick_test_ref_path_clicked = Some(TestRefSlot::Existing(i));
                            }
                            if ui.button("Remove").clicked() {
                                remove_test_ref = Some(i);
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
                        if form.adding_test_ref {
                            egui::Modal::new(egui::Id::new("add_test_reference_dialog")).show(
                                ui.ctx(),
                                |ui| {
                                    ui.heading("Add Test Procedure");
                                    let (_, auto, pick_clicked) = render_test_ref_fields(
                                        ui,
                                        &mut form.new_test_ref,
                                        self.tree.as_ref(),
                                    );
                                    if let Some(target) = auto {
                                        test_ref_auto_commit_clicked =
                                            Some((TestRefSlot::New, target));
                                    }
                                    if pick_clicked {
                                        pick_test_ref_path_clicked = Some(TestRefSlot::New);
                                    }
                                    ui.horizontal(|ui| {
                                        if ui.button("Add test procedure").clicked() {
                                            form.tests.push(form.new_test_ref.clone());
                                            let new_index = form.tests.len() - 1;
                                            // Auto-populate the new row's commit
                                            // exactly as if its own "Auto" button
                                            // had been clicked, so the user
                                            // doesn't have to do that as a
                                            // manual follow-up step.
                                            if let Some(tree) = self.tree.as_ref() {
                                                let path = form.tests[new_index].path.clone();
                                                if let Some(target) =
                                                    flatten_leaf_paths(tree, EntryKind::Test)
                                                        .into_iter()
                                                        .find(|target| {
                                                            absolute_reference_path(
                                                                target,
                                                                leaf_kind_segment(LeafKind::Test),
                                                            ) == path
                                                        })
                                                {
                                                    test_ref_auto_commit_clicked = Some((
                                                        TestRefSlot::Existing(new_index),
                                                        target,
                                                    ));
                                                }
                                            }
                                            form.new_test_ref = TestRefDraft::default();
                                            form.adding_test_ref = false;
                                            form.edited = true;
                                        }
                                        if ui.button("Cancel").clicked() {
                                            form.new_test_ref = TestRefDraft::default();
                                            form.adding_test_ref = false;
                                        }
                                    });
                                },
                            );
                        } else if ui.button("Add test procedure").clicked() {
                            form.adding_test_ref = true;
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
                    section_gap(ui);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label("Results:");
                        if form.results.is_empty() {
                            ui.label("No results reference this requirement yet.");
                        }
                        for result in &form.results {
                            // Unlike Dependencies/Test references above, a
                            // result's own name is already known without
                            // parsing a reference string — its owning
                            // requirement is `form.editing_target` itself
                            // (structural nesting, not a resolved
                            // reference), so this is always clickable once
                            // the requirement itself has a stable path.
                            if ui
                                .link(format!("{} ({:?})", result.title, result.status))
                                .clicked()
                                && let Some(requirement) = form.editing_target.clone()
                            {
                                navigate_clicked =
                                    Some(gui_core::EntryPath::Result(gui_core::ResultPath {
                                        requirement,
                                        name: result.name.clone(),
                                    }));
                            }
                        }
                        // Unlike Dependencies/Test references/Local
                        // attachments, this doesn't mutate the requirement
                        // itself — it opens the Create Result dialog, which
                        // creates a separate Result entity referencing this
                        // requirement. So it's offered in the read-only
                        // viewer too, and always shown (not just when there
                        // are no results yet) so a second/third result can
                        // be added just as easily as the first.
                        if form.tests.is_empty() {
                            ui.label("Add a test procedure above before creating a result.");
                        } else if ui.button("Create new result").clicked() {
                            create_result_clicked = true;
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
                // Pinned at the bottom, below every other section — a
                // history log is background information to check on
                // demand, not something that belongs above the entry's own
                // content.
                if let Some(target) = &form.editing_target {
                    ui.separator();
                    let entry_path = gui_core::EntryPath::Requirement(target.clone());
                    render_commit_log_section(
                        ui,
                        &entry_path,
                        self.commit_log_target.as_ref(),
                        self.commit_log.as_deref(),
                        self.commit_log_error.as_deref(),
                        self.dirty,
                        &mut commit_log_expand_requested,
                        &mut commit_log_commit_clicked,
                    );
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
        } else if create_result_clicked {
            self.create_result_clicked();
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
        if let Some(target) = navigate_clicked {
            self.select(target);
        } else if let Some(module) = navigate_module_clicked {
            self.select_module(module);
        }
        if let Some(word) = spellcheck_add_to_dictionary {
            self.add_spellcheck_word_to_dictionary(word);
        }
        if let Some(target) = commit_log_expand_requested {
            self.commit_log_section_expanded(target);
        }
        if let Some((target, commit)) = commit_log_commit_clicked {
            self.commit_files_dialog_opened(target, commit);
        }
    }

    fn render_test_form(&mut self, ui: &mut egui::Ui) {
        self.ensure_spell_checker_started();
        let mut create_clicked = false;
        let mut cancel_clicked = false;
        let mut edit_clicked = false;
        let mut delete_clicked = false;
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        let mut add_template_clicked = false;
        let mut remove_template: Option<PathBuf> = None;
        let mut recreate_clicked = false;
        // See the Requirement form's own comment on this.
        let mut spellcheck_add_to_dictionary: Option<String> = None;
        // See the Requirement form's own comment on this.
        let mut commit_log_expand_requested: Option<gui_core::EntryPath> = None;
        // See the Requirement form's own comment on this.
        let mut commit_log_commit_clicked: Option<(gui_core::EntryPath, String)> = None;
        {
            let EditorState::NewTest(form) = &mut self.editor else {
                return;
            };
            let editing = form.editing_target.is_some();
            let read_only = form.read_only;
            ui.horizontal(|ui| {
                ui.heading(if read_only {
                    "Test Procedure"
                } else if editing {
                    "Edit Test Procedure"
                } else {
                    "New Test Procedure"
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
                    ui.label("Identifier:");
                    if read_only {
                        ui.label(&form.name);
                    } else if ui
                        .add_enabled(!editing, egui::TextEdit::singleline(&mut form.name))
                        .changed()
                    {
                        form.edited = true;
                    }
                });
                // See the Requirement form's own comment on this.
                if let Some(target) = &form.editing_target {
                    ui.horizontal(|ui| {
                        ui.label("Path:");
                        ui.add(
                            egui::Label::new(absolute_reference_path(
                                target,
                                leaf_kind_segment(LeafKind::Test),
                            ))
                            .selectable(true),
                        );
                    });
                }
                if read_only {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        ui.label(&form.title);
                    });
                    render_grouped_text(ui, "Test procedure text:", &form.test_text);
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
                        let (changed, added) = spellchecked_singleline(
                            ui,
                            &mut form.title,
                            SpellCtx {
                                checker: &self.spell_checker,
                                enabled: self.config.spellcheck_enabled,
                                custom_words: &self.config.spellcheck_custom_words,
                                cache: &mut form.title_spell,
                                popup: &mut form.spell_popup,
                                field: SpellField::Title,
                            },
                        );
                        if changed {
                            form.edited = true;
                        }
                        if spellcheck_add_to_dictionary.is_none() {
                            spellcheck_add_to_dictionary = added;
                        }
                        if ui
                            .button(icons::REGENERATE_TITLE)
                            .on_hover_text("Regenerate title from identifier")
                            .clicked()
                        {
                            form.title = title_case_from_name(&form.name);
                            form.edited = true;
                        }
                    });
                    // See the Requirement form's own comment on `entry_id` —
                    // same reasoning, for this test's `resizable_multiline`.
                    let entry_id = entry_id_salt(&form.editing_target);
                    ui.label("Test procedure text:");
                    let (changed, added) = resizable_multiline(
                        ui,
                        &format!("test_text:{entry_id}"),
                        &mut form.test_text,
                        SpellCtx {
                            checker: &self.spell_checker,
                            enabled: self.config.spellcheck_enabled,
                            custom_words: &self.config.spellcheck_custom_words,
                            cache: &mut form.test_text_spell,
                            popup: &mut form.spell_popup,
                            field: SpellField::TestText,
                        },
                    );
                    if changed {
                        form.edited = true;
                    }
                    if spellcheck_add_to_dictionary.is_none() {
                        spellcheck_add_to_dictionary = added;
                    }
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
                // See the Requirement form's own comment on why this is
                // pinned at the bottom.
                if let Some(target) = &form.editing_target {
                    ui.separator();
                    let entry_path = gui_core::EntryPath::Test(target.clone());
                    render_commit_log_section(
                        ui,
                        &entry_path,
                        self.commit_log_target.as_ref(),
                        self.commit_log.as_deref(),
                        self.commit_log_error.as_deref(),
                        self.dirty,
                        &mut commit_log_expand_requested,
                        &mut commit_log_commit_clicked,
                    );
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
        if let Some(word) = spellcheck_add_to_dictionary {
            self.add_spellcheck_word_to_dictionary(word);
        }
        if let Some(target) = commit_log_expand_requested {
            self.commit_log_section_expanded(target);
        }
        if let Some((target, commit)) = commit_log_commit_clicked {
            self.commit_files_dialog_opened(target, commit);
        }
    }

    fn render_result_form(&mut self, ui: &mut egui::Ui) {
        self.ensure_spell_checker_started();
        let mut create_clicked = false;
        let mut cancel_clicked = false;
        let mut edit_clicked = false;
        let mut delete_clicked = false;
        let mut add_attachment_clicked = false;
        let mut remove_attachment: Option<PathBuf> = None;
        let mut open_picker: Option<PathPickerTarget> = None;
        let mut refresh_stale_result_reference_clicked = false;
        // See the Requirement form's own comment on this.
        let mut spellcheck_add_to_dictionary: Option<String> = None;
        // See the Requirement form's own comment on this.
        let mut commit_log_expand_requested: Option<gui_core::EntryPath> = None;
        // See the Requirement form's own comment on this.
        let mut commit_log_commit_clicked: Option<(gui_core::EntryPath, String)> = None;
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
                    // Only when there's actually something for it to fix
                    // — see `EntryDetail::Result::stale`'s own doc comment.
                    if form.stale {
                        let busy = form.pending_request.is_some();
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new((
                                    icons::UPDATE_STALE_REFERENCES,
                                    "Update Stale Reference",
                                )),
                            )
                            .clicked()
                        {
                            refresh_stale_result_reference_clicked = true;
                        }
                    }
                } else {
                    // See the Requirement form's own comment on why this
                    // lives next to the heading rather than only at the
                    // bottom.
                    let busy = form.pending_request.is_some();
                    // A create-mode form has no requirement yet without a
                    // pick — `build_command`'s `AddResult` branch relies on
                    // this to guarantee `form.requirement` is `Some`.
                    let can_submit = editing || form.requirement.is_some();
                    let button_label = if editing { "Save" } else { "Create" };
                    if ui
                        .add_enabled(!busy && can_submit, egui::Button::new(button_label))
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
                    ui.label("Identifier:");
                    if read_only {
                        ui.label(&form.name);
                    } else if ui
                        .add_enabled(!editing, egui::TextEdit::singleline(&mut form.name))
                        .changed()
                    {
                        form.edited = true;
                    }
                });
                // See the Requirement form's own comment on this.
                if let Some(target) = &form.editing_target {
                    ui.horizontal(|ui| {
                        ui.label("Path:");
                        ui.add(egui::Label::new(absolute_result_path(target)).selectable(true));
                    });
                }
                if read_only {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        ui.label(&form.title);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Requirement:");
                        ui.label(
                            form.requirement
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Requirement commit:");
                        ui.label(&form.requirement_commit);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Test procedure path:");
                        ui.label(&form.test_path);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Test procedure commit:");
                        ui.label(&form.test_commit);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.label(format!("{:?}", form.status));
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        let (changed, added) = spellchecked_singleline(
                            ui,
                            &mut form.title,
                            SpellCtx {
                                checker: &self.spell_checker,
                                enabled: self.config.spellcheck_enabled,
                                custom_words: &self.config.spellcheck_custom_words,
                                cache: &mut form.title_spell,
                                popup: &mut form.spell_popup,
                                field: SpellField::Title,
                            },
                        );
                        if changed {
                            form.edited = true;
                        }
                        if spellcheck_add_to_dictionary.is_none() {
                            spellcheck_add_to_dictionary = added;
                        }
                        if ui
                            .button(icons::REGENERATE_TITLE)
                            .on_hover_text("Regenerate title from identifier")
                            .clicked()
                        {
                            form.title = title_case_from_name(&form.name);
                            form.edited = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Requirement:");
                        ui.label(
                            form.requirement
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_else(|| "(none picked)".to_string()),
                        );
                        // A result's owning requirement is structural
                        // (it's saved nested under it — see
                        // `ResultFormState::requirement`'s doc comment),
                        // fixed once created: the picker only opens for a
                        // create-mode form (`!editing`), same "Identifier"
                        // treatment above. Opens the shared path-picker
                        // modal (`GuiApp::path_picker_dialog`) rather than
                        // an inline `ComboBox` — a project with enough
                        // requirements would otherwise overflow a
                        // `ComboBox` popup right off the screen, with no
                        // way to search it down to the one wanted.
                        if !editing && self.tree.is_some() && ui.button("Pick…").clicked() {
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
                        ui.label("Test procedure path:");
                        if ui.text_edit_singleline(&mut form.test_path).changed() {
                            form.edited = true;
                        }
                        if self.tree.is_some() && ui.button("Pick…").clicked() {
                            open_picker = Some(PathPickerTarget::ResultTestPath);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Test procedure commit:");
                        if ui.text_edit_singleline(&mut form.test_commit).changed() {
                            form.edited = true;
                        }
                    });
                    ui.label(
                        "Commits aren't picked automatically yet — copy the target's \
                     current commit by hand (see README's Open questions).",
                    );
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        if ui
                            .selectable_label(
                                matches!(form.status, gui_core::StatusV1::Pass),
                                "Pass",
                            )
                            .clicked()
                        {
                            form.status = gui_core::StatusV1::Pass;
                            form.edited = true;
                        }
                        if ui
                            .selectable_label(
                                matches!(form.status, gui_core::StatusV1::Fail),
                                "Fail",
                            )
                            .clicked()
                        {
                            form.status = gui_core::StatusV1::Fail;
                            form.edited = true;
                        }
                        if ui
                            .selectable_label(
                                matches!(form.status, gui_core::StatusV1::Incomplete),
                                "Incomplete",
                            )
                            .clicked()
                        {
                            form.status = gui_core::StatusV1::Incomplete;
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
                        if let Some(error) = &form.local_pool_error {
                            ui.colored_label(egui::Color32::RED, error);
                        }
                    }
                }
                // See the Requirement form's own comment on why this is
                // pinned at the bottom.
                if let Some(target) = &form.editing_target {
                    ui.separator();
                    let entry_path = gui_core::EntryPath::Result(target.clone());
                    render_commit_log_section(
                        ui,
                        &entry_path,
                        self.commit_log_target.as_ref(),
                        self.commit_log.as_deref(),
                        self.commit_log_error.as_deref(),
                        self.dirty,
                        &mut commit_log_expand_requested,
                        &mut commit_log_commit_clicked,
                    );
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
        if refresh_stale_result_reference_clicked {
            self.refresh_stale_result_reference_clicked();
        }
        if let Some(word) = spellcheck_add_to_dictionary {
            self.add_spellcheck_word_to_dictionary(word);
        }
        if let Some(target) = commit_log_expand_requested {
            self.commit_log_section_expanded(target);
        }
        if let Some((target, commit)) = commit_log_commit_clicked {
            self.commit_files_dialog_opened(target, commit);
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
                ui.label("Identifier:");
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
                ui.heading(if is_root { "Project" } else { "Module" });
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
            // See the Requirement form's own comment on giving the
            // identifier its own row rather than folding it into the
            // heading.
            ui.horizontal(|ui| {
                ui.label("Identifier:");
                if form.read_only {
                    ui.label(&form.display_name);
                } else if ui.text_edit_singleline(&mut form.new_name).changed() {
                    form.edited = true;
                }
            });
            // See the Requirement form's own comment on this.
            if !is_root {
                let path_label = form
                    .path
                    .iter()
                    .map(EntryName::as_str)
                    .collect::<Vec<_>>()
                    .join("/");
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    ui.add(egui::Label::new(path_label).selectable(true));
                });
            }
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
                            ui.label(format!("Test Procedures: {}", summary.test_count));
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
                } else if let Some(error) = &form.error {
                    ui.colored_label(egui::Color32::RED, error);
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
                ui.label("Identifier:");
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
        let mut diff_clicked: Option<PathBuf> = None;

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
                // Reserve the same footprint the loaded form below would
                // take — see `render_diff_dialog`'s matching comment.
                ui.set_min_height(resizable_budget);
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
                            None,
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
                                        if ui.link(path.display().to_string()).clicked() {
                                            diff_clicked = Some(path.clone());
                                        }
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
        } else if let Some(path) = diff_clicked {
            self.diff_file_clicked(path);
        }
    }

    /// The per-commit file-list modal — opened by clicking a commit's hash
    /// link in a view screen's "Commit history" section
    /// (`render_commit_log_section`). Same structure as
    /// `render_commit_all_dialog` above, minus the commit-message box and
    /// Commit button — this modal is read-only, just a heading naming the
    /// commit and a scrolled list of files it changed (scoped to agree
    /// with the log itself — see `Command::GetCommitFiles`'s own doc
    /// comment), each a link opening the diff modal for that file.
    pub(crate) fn render_commit_files_dialog(&mut self, ui: &mut egui::Ui) {
        if self.commit_files_dialog.is_none() {
            return;
        }

        let mut close_clicked = false;
        let mut diff_clicked: Option<PathBuf> = None;

        let screen_rect = ui.ctx().content_rect();
        let max_height = (screen_rect.height() - 120.0).max(200.0);
        let width = screen_rect.width() * 0.5;

        egui::Modal::new(egui::Id::new("commit_files_dialog")).show(ui.ctx(), |ui| {
            let Some(dialog) = &self.commit_files_dialog else {
                return;
            };
            ui.set_width(width);
            ui.heading(format!(
                "Commit {}",
                &dialog.commit[..dialog.commit.len().min(8)]
            ));
            ui.separator();

            if dialog.loading {
                // Reserve the same footprint the loaded file list below
                // would take — see `render_diff_dialog`'s matching comment.
                ui.set_min_height(max_height);
                ui.label("Loading…");
            } else if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            } else {
                ui.label(format!("Files changed ({}):", dialog.files.len()));
                egui::ScrollArea::vertical()
                    .max_height(max_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if dialog.files.is_empty() {
                            ui.label("No files.");
                        } else {
                            for path in &dialog.files {
                                if ui.link(path.display().to_string()).clicked() {
                                    diff_clicked = Some(path.clone());
                                }
                            }
                        }
                    });
            }

            ui.separator();
            if ui.button("Close").clicked() {
                close_clicked = true;
            }
        });

        if close_clicked {
            self.commit_files_dialog_closed();
        } else if let Some(path) = diff_clicked {
            self.commit_file_diff_clicked(path);
        }
    }

    /// The "View diff" modal — opened by clicking a file in
    /// `render_commit_all_dialog`'s file list, or by clicking a file in
    /// `render_commit_files_dialog`'s (`DiffDialogState::commit` tells
    /// these two apart — see its own doc comment). Renders the unified diff
    /// `Command::GetDiff` returned one line at a time, colored red/green
    /// via `theme_colors::diff_line_colors` (which returns `None` for
    /// context/header lines, left in the theme's ordinary text color) so
    /// it reads correctly in both light and dark mode. `ScrollArea::both`
    /// rather than `::vertical` since diff lines routinely run wider than
    /// the modal and shouldn't wrap (wrapping would misalign the +/-
    /// markers from the text they annotate).
    pub(crate) fn render_diff_dialog(&mut self, ui: &mut egui::Ui) {
        if self.diff_dialog.is_none() {
            return;
        }

        let mut close_clicked = false;
        let screen_rect = ui.ctx().content_rect();
        let width = screen_rect.width() * 0.7;
        let max_height = (screen_rect.height() - 120.0).max(200.0);

        egui::Modal::new(egui::Id::new("diff_dialog")).show(ui.ctx(), |ui| {
            let Some(dialog) = &self.diff_dialog else {
                return;
            };
            ui.set_width(width);
            let heading = match &dialog.commit {
                Some(commit) => format!(
                    "Diff: {} @ {}",
                    dialog.path.display(),
                    &commit[..commit.len().min(8)]
                ),
                None => format!("Diff: {}", dialog.path.display()),
            };
            ui.heading(heading);
            ui.separator();

            if dialog.loading {
                // Reserve the same footprint the loaded diff below would
                // take, so the modal opens at its eventual size instead of
                // popping larger once `GetDiff`/`GetCommitFileDiff`
                // completes (that completion latency previously read as
                // the modal "growing" once the diff arrived).
                ui.set_min_height(max_height);
                ui.label("Loading…");
            } else if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            } else if dialog.diff.trim().is_empty() {
                ui.label("No textual diff available (binary file, or no changes).");
            } else {
                egui::ScrollArea::both()
                    .max_height(max_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        let dark_mode = ui.visuals().dark_mode;
                        for line in dialog.diff.lines() {
                            let kind = theme_colors::classify_diff_line(line);
                            let mut text = egui::RichText::new(line).monospace();
                            if let Some((fg, bg)) = theme_colors::diff_line_colors(dark_mode, kind)
                            {
                                text = text.color(fg).background_color(bg);
                            }
                            ui.label(text);
                        }
                    });
            }

            ui.separator();
            if ui.button("Close").clicked() {
                close_clicked = true;
            }
        });

        if close_clicked {
            self.diff_dialog_closed();
        }
    }

    /// The "Push" modal — opens in a confirm state (see `PushDialogState`'s
    /// own doc comment), shows "Pushing…" while `Command::Push` is in
    /// flight, then either the combined git output (success) or the error
    /// (failure), with the confirm button still available on failure to
    /// retry. No auto-close on success, unlike "Commit all changes" — the
    /// whole point is showing the user push's status/output until they
    /// dismiss it. The confirm state also shows a preview of what would
    /// actually be pushed (`render_unpushed_commits_preview`), including a
    /// warning when there's nothing to push.
    pub(crate) fn render_push_dialog(&mut self, ui: &mut egui::Ui) {
        if self.push_dialog.is_none() {
            return;
        }

        let mut close_clicked = false;
        let mut push_clicked = false;
        let screen_rect = ui.ctx().content_rect();
        let width = screen_rect.width() * 0.6;
        let max_height = (screen_rect.height() - 120.0).max(200.0);

        egui::Modal::new(egui::Id::new("push_dialog")).show(ui.ctx(), |ui| {
            let Some(dialog) = &self.push_dialog else {
                return;
            };
            ui.set_width(width);
            ui.heading("Push");
            ui.separator();

            if dialog.pushing {
                ui.label("Pushing…");
            } else if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            } else if let Some(output) = &dialog.output {
                egui::ScrollArea::both()
                    .max_height(max_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        ui.label(egui::RichText::new(output).monospace());
                    });
            } else {
                ui.label("Push the current branch to its remote?");
                ui.add_space(8.0);
                render_unpushed_commits_preview(ui, dialog);
            }

            ui.separator();
            ui.horizontal(|ui| {
                if dialog.output.is_none() {
                    if ui
                        .add_enabled(
                            !dialog.pushing,
                            egui::Button::new(if dialog.pushing { "Pushing…" } else { "Push" }),
                        )
                        .clicked()
                    {
                        push_clicked = true;
                    }
                }
                let close_label = if dialog.output.is_some() || dialog.error.is_some() {
                    "Close"
                } else {
                    "Cancel"
                };
                if ui.button(close_label).clicked() {
                    close_clicked = true;
                }
            });
        });

        if push_clicked {
            self.push_dialog_push_clicked();
        } else if close_clicked {
            self.push_dialog_closed();
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
                LeafKind::Requirement => "Pick a requirement",
                LeafKind::Test => "Pick a test procedure",
            });
            ui.text_edit_singleline(&mut dialog.filter);

            if matches!(
                dialog.target,
                PathPickerTarget::Dependency(_) | PathPickerTarget::TestReference(_)
            ) {
                ui.horizontal(|ui| {
                    ui.radio_value(&mut dialog.scope, PathPickerScope::All, "All");
                    ui.radio_value(
                        &mut dialog.scope,
                        PathPickerScope::ThisModule,
                        "This module",
                    );
                    ui.radio_value(&mut dialog.scope, PathPickerScope::Submodules, "Submodules");
                });
            }

            let kind_segment = leaf_kind_segment(dialog.kind);
            let filter = dialog.filter.to_lowercase();

            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    let mut any_shown = false;
                    for target in
                        scoped_leaf_paths(tree, dialog.kind.into(), dialog.scope, &dialog.owning_module)
                    {
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

    /// The tree's right-click "Duplicate" name prompt — see
    /// `DuplicateRequirementState`'s own doc comment. Enter in the name
    /// field submits the same as clicking "Duplicate", but only when that
    /// button would actually be enabled — same guard shape as the Recreate
    /// dialogs' own `enter_pressed_in_name_field` handling.
    pub(crate) fn render_duplicate_requirement_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(dialog) = self.duplicate_requirement_dialog.clone() else {
            return;
        };

        let mut new_name = dialog.new_name;
        let mut regenerate_title = dialog.regenerate_title;
        let mut confirmed = false;
        let mut cancelled = false;
        let mut enter_pressed_in_name_field = false;
        let busy = dialog.pending_request.is_some();
        egui::Modal::new(egui::Id::new("duplicate_requirement_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Duplicate Requirement");
            ui.label(format!(
                "Creates a copy of \"{}\" in the same module under a new name.",
                dialog.source.name
            ));
            ui.horizontal(|ui| {
                ui.label("New name:");
                let name_response =
                    ui.add_enabled(!busy, egui::TextEdit::singleline(&mut new_name));
                if name_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    enter_pressed_in_name_field = true;
                }
            });
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut regenerate_title, "Regenerate title from new name"),
            );
            let name_taken = dialog.existing_names.contains(new_name.trim());
            if name_taken {
                ui.colored_label(
                    egui::Color32::RED,
                    format!("\"{}\" already exists in this module.", new_name.trim()),
                );
            }
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            let can_confirm = !busy && !new_name.trim().is_empty() && !name_taken;
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(can_confirm, egui::Button::new("Duplicate"))
                    .clicked()
                {
                    confirmed = true;
                }
                if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                    cancelled = true;
                }
            });
            if enter_pressed_in_name_field && can_confirm {
                confirmed = true;
            }
        });

        if confirmed {
            if let Some(dialog) = &mut self.duplicate_requirement_dialog {
                dialog.new_name = new_name;
                dialog.regenerate_title = regenerate_title;
            }
            self.duplicate_requirement_confirmed();
        } else if cancelled {
            self.duplicate_requirement_cancelled();
        } else if let Some(dialog) = &mut self.duplicate_requirement_dialog {
            dialog.new_name = new_name;
            dialog.regenerate_title = regenerate_title;
        }
    }

    /// The requirement view's "Create new result" prompt — opened by
    /// `GuiApp::create_result_clicked` from the Results section in
    /// `render_requirement_form`. See `CreateResultDialogState`'s own doc
    /// comment for why the test picker only offers this requirement's own
    /// `tests`, and why the commit fields are pre-filled automatically but
    /// stay plain editable text (the underlying fetch can fail silently,
    /// same as the dependency/test-reference rows' own "Auto" button).
    pub(crate) fn render_create_result_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(dialog) = self.create_result_dialog.clone() else {
            return;
        };

        let mut name = dialog.name;
        let mut title = dialog.title;
        let mut regenerate_title = dialog.regenerate_title;
        let mut test_index = dialog.test_index;
        let mut status = dialog.status.clone();
        let mut requirement_commit = dialog.requirement_commit.clone();
        let mut test_commit = dialog.test_commit.clone();
        let mut confirmed = false;
        let mut cancelled = false;
        let busy = dialog.pending_request.is_some();
        egui::Modal::new(egui::Id::new("create_result_dialog")).show(ui.ctx(), |ui| {
            ui.heading("New Result");
            ui.label(format!("For requirement \"{}\".", dialog.requirement.name));
            ui.horizontal(|ui| {
                ui.label("Identifier:");
                ui.add_enabled(!busy, egui::TextEdit::singleline(&mut name));
            });
            // Kept in sync with `name` every frame while checked, same
            // "checkbox drives a derived field" idea as the Duplicate/
            // Recreate dialogs' own `regenerate_title` — here it's also
            // visible live (not just applied at confirm), cheap enough to
            // just show.
            if regenerate_title {
                title = title_case_from_name(&name);
            }
            ui.horizontal(|ui| {
                ui.label("Title:");
                ui.add_enabled(
                    !busy && !regenerate_title,
                    egui::TextEdit::singleline(&mut title),
                );
            });
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut regenerate_title, "Generate title from identifier"),
            );
            ui.horizontal(|ui| {
                ui.label("Test procedure:");
                egui::ComboBox::new("create_result_test_picker", "")
                    .selected_text(
                        dialog
                            .tests
                            .get(test_index)
                            .map(|t| t.path.as_str())
                            .unwrap_or(""),
                    )
                    .show_ui(ui, |ui| {
                        for (i, test_ref) in dialog.tests.iter().enumerate() {
                            ui.selectable_value(&mut test_index, i, test_ref.path.as_str());
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("Status:");
                if ui
                    .selectable_label(matches!(status, gui_core::StatusV1::Pass), "Pass")
                    .clicked()
                {
                    status = gui_core::StatusV1::Pass;
                }
                if ui
                    .selectable_label(matches!(status, gui_core::StatusV1::Fail), "Fail")
                    .clicked()
                {
                    status = gui_core::StatusV1::Fail;
                }
                if ui
                    .selectable_label(
                        matches!(status, gui_core::StatusV1::Incomplete),
                        "Incomplete",
                    )
                    .clicked()
                {
                    status = gui_core::StatusV1::Incomplete;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Requirement commit:");
                ui.add_enabled(!busy, egui::TextEdit::singleline(&mut requirement_commit));
                if dialog.requirement_commit_pending.is_some() {
                    ui.label("(resolving…)");
                }
            });
            ui.horizontal(|ui| {
                ui.label("Test commit:");
                ui.add_enabled(!busy, egui::TextEdit::singleline(&mut test_commit));
                if dialog.test_commit_pending.is_some() {
                    ui.label("(resolving…)");
                }
            });
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            let can_confirm = !busy && !name.trim().is_empty() && !dialog.tests.is_empty();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(can_confirm, egui::Button::new("Create result"))
                    .clicked()
                {
                    confirmed = true;
                }
                if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                    cancelled = true;
                }
            });
        });

        let test_changed = test_index != dialog.test_index;
        // Keep the identifier (and, transitively, the title when it's
        // still following the identifier) in sync with the selected test
        // procedure — but only when the field still holds the
        // auto-generated default for the *previously* selected test, so
        // switching tests never clobbers an identifier the user typed by
        // hand. Same "don't stomp a manual edit" precedent as
        // `regenerate_title` itself.
        if test_changed
            && let Some(old_test) = dialog.tests.get(dialog.test_index)
            && let Some(new_test) = dialog.tests.get(test_index)
        {
            let today = today_iso_date();
            if name == default_result_name(&today, old_test) {
                name = default_result_name(&today, new_test);
                if regenerate_title {
                    title = title_case_from_name(&name);
                }
            }
        }
        if confirmed {
            if let Some(dialog) = &mut self.create_result_dialog {
                dialog.name = name;
                dialog.title = title;
                dialog.regenerate_title = regenerate_title;
                dialog.test_index = test_index;
                dialog.status = status;
                dialog.requirement_commit = requirement_commit;
                dialog.test_commit = test_commit;
            }
            self.create_result_confirmed();
        } else if cancelled {
            self.create_result_cancelled();
        } else {
            if let Some(dialog) = &mut self.create_result_dialog {
                dialog.name = name;
                dialog.title = title;
                dialog.regenerate_title = regenerate_title;
                dialog.test_index = test_index;
                dialog.status = status;
                dialog.requirement_commit = requirement_commit;
                dialog.test_commit = test_commit;
            }
            if test_changed {
                self.create_result_fetch_test_commit(test_index);
            }
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
        let mut regenerate_title = dialog.regenerate_title;
        let mut reference_choices = dialog.reference_choices;
        let mut confirmed = false;
        let mut cancelled = false;
        let mut enter_pressed_in_name_field = false;
        let busy = dialog.pending_request.is_some();
        let button_label = if dialog.repairing {
            "Retry Repair"
        } else {
            "Recreate"
        };
        egui::Modal::new(egui::Id::new("recreate_requirement_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Recreate Requirement");
            ui.label(format!(
                "This deletes \"{}\" and creates a new requirement with the same contents under a new stable name.",
                dialog.target.name
            ));
            ui.horizontal(|ui| {
                ui.label("New name:");
                let name_response =
                    ui.add_enabled(!dialog.deleted, egui::TextEdit::singleline(&mut new_name));
                if name_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    enter_pressed_in_name_field = true;
                }
            });
            ui.add_enabled(
                !dialog.deleted,
                egui::Checkbox::new(&mut regenerate_title, "Regenerate title from new name"),
            );
            let text_empty = !dialog.deleted && dialog.requirement.requirement_text.trim().is_empty();
            if text_empty {
                ui.colored_label(
                    egui::Color32::RED,
                    "Requirement text is empty — fix it before recreating.",
                );
            }
            if !reference_choices.is_empty() {
                ui.label(format!(
                    "This will break {} reference{} into it. Choose how to handle each one:",
                    reference_choices.len(),
                    if reference_choices.len() == 1 { "" } else { "s" }
                ));
                egui::Grid::new("recreate_requirement_references_grid").striped(true).show(ui, |ui| {
                    for (index, (site, action)) in reference_choices.iter_mut().enumerate() {
                        ui.label(format!("{} ({})", site.referrer, reference_site_kind_label(&site.kind)));
                        egui::ComboBox::new(("recreate_requirement_reference_action", index), "")
                            .selected_text(match action {
                                ReferenceAction::Repair => "Repair",
                                ReferenceAction::Remove => "Remove",
                                ReferenceAction::Ignore => "Ignore",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(action, ReferenceAction::Repair, "Repair");
                                ui.selectable_value(action, ReferenceAction::Remove, "Remove");
                                ui.selectable_value(action, ReferenceAction::Ignore, "Ignore");
                            });
                        ui.end_row();
                    }
                });
            }
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            let name_unchanged = !dialog.deleted && new_name.trim() == dialog.target.name.as_str();
            let can_confirm = !busy && !new_name.trim().is_empty() && !name_unchanged && !text_empty;
            ui.horizontal(|ui| {
                if ui.add_enabled(can_confirm, egui::Button::new(button_label)).clicked() {
                    confirmed = true;
                }
                if ui.add_enabled(!busy && !dialog.deleted, egui::Button::new("Cancel")).clicked() {
                    cancelled = true;
                }
            });
            // Enter in the "New name:" field submits the same as clicking
            // the Recreate/Retry Repair button, but only when that button
            // would actually be enabled — a `key_pressed` check alone would
            // otherwise let Enter bypass the empty-name/unchanged-name/
            // empty-text guards `can_confirm` exists to enforce.
            if enter_pressed_in_name_field && can_confirm {
                confirmed = true;
            }
        });

        if confirmed {
            if let Some(dialog) = &mut self.recreate_requirement_dialog {
                dialog.new_name = new_name;
                dialog.regenerate_title = regenerate_title;
                dialog.reference_choices = reference_choices;
            }
            self.recreate_requirement_confirmed();
        } else if cancelled {
            self.recreate_requirement_cancelled();
        } else if let Some(dialog) = &mut self.recreate_requirement_dialog {
            dialog.new_name = new_name;
            dialog.regenerate_title = regenerate_title;
            dialog.reference_choices = reference_choices;
        }
    }

    pub(crate) fn render_recreate_test_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(dialog) = self.recreate_test_dialog.clone() else {
            return;
        };

        let mut new_name = dialog.new_name;
        let mut reference_choices = dialog.reference_choices;
        let mut confirmed = false;
        let mut cancelled = false;
        let mut enter_pressed_in_name_field = false;
        let busy = dialog.pending_request.is_some();
        let button_label = if dialog.repairing {
            "Retry Repair"
        } else {
            "Recreate"
        };
        egui::Modal::new(egui::Id::new("recreate_test_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Recreate Test Procedure");
            ui.label(format!(
                "This deletes \"{}\" and creates a new test procedure with the same contents under a new stable name.",
                dialog.target.name
            ));
            ui.horizontal(|ui| {
                ui.label("New name:");
                let name_response =
                    ui.add_enabled(!dialog.deleted, egui::TextEdit::singleline(&mut new_name));
                if name_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    enter_pressed_in_name_field = true;
                }
            });
            let text_empty = !dialog.deleted && dialog.test.test_text.trim().is_empty();
            if text_empty {
                ui.colored_label(
                    egui::Color32::RED,
                    "Test procedure text is empty — fix it before recreating.",
                );
            }
            if !reference_choices.is_empty() {
                ui.label(format!(
                    "This will break {} reference{} into it. Choose how to handle each one:",
                    reference_choices.len(),
                    if reference_choices.len() == 1 { "" } else { "s" }
                ));
                egui::Grid::new("recreate_test_references_grid").striped(true).show(ui, |ui| {
                    for (index, (site, action)) in reference_choices.iter_mut().enumerate() {
                        ui.label(format!("{} ({})", site.referrer, reference_site_kind_label(&site.kind)));
                        egui::ComboBox::new(("recreate_test_reference_action", index), "")
                            .selected_text(match action {
                                ReferenceAction::Repair => "Repair",
                                ReferenceAction::Remove => "Remove",
                                ReferenceAction::Ignore => "Ignore",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(action, ReferenceAction::Repair, "Repair");
                                ui.selectable_value(action, ReferenceAction::Remove, "Remove");
                                ui.selectable_value(action, ReferenceAction::Ignore, "Ignore");
                            });
                        ui.end_row();
                    }
                });
            }
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            let name_unchanged = !dialog.deleted && new_name.trim() == dialog.target.name.as_str();
            let can_confirm = !busy && !new_name.trim().is_empty() && !name_unchanged && !text_empty;
            ui.horizontal(|ui| {
                if ui.add_enabled(can_confirm, egui::Button::new(button_label)).clicked() {
                    confirmed = true;
                }
                if ui.add_enabled(!busy && !dialog.deleted, egui::Button::new("Cancel")).clicked() {
                    cancelled = true;
                }
            });
            // Enter in the "New name:" field submits the same as clicking
            // the Recreate/Retry Repair button, but only when that button
            // would actually be enabled — see the requirement dialog's own
            // comment on this same guard.
            if enter_pressed_in_name_field && can_confirm {
                confirmed = true;
            }
        });

        if confirmed {
            if let Some(dialog) = &mut self.recreate_test_dialog {
                dialog.new_name = new_name;
                dialog.reference_choices = reference_choices;
            }
            self.recreate_test_confirmed();
        } else if cancelled {
            self.recreate_test_cancelled();
        } else if let Some(dialog) = &mut self.recreate_test_dialog {
            dialog.new_name = new_name;
            dialog.reference_choices = reference_choices;
        }
    }

    /// The broken-references modal opened when a module rename's
    /// `Command::FindReferences` pre-check finds sites that would break —
    /// see `GuiApp::broken_references_dialog`'s own doc comment. One row per
    /// `ReferenceSite`, a `Repair`/`Remove`/`Ignore` choice each (default
    /// `Repair`), Confirm sends the rename with those choices attached.
    pub(crate) fn render_broken_references_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(dialog) = self.broken_references_dialog.clone() else {
            return;
        };

        let mut choices = dialog.choices;
        let mut confirmed = false;
        let mut cancelled = false;
        let busy = dialog.pending_request.is_some();
        egui::Modal::new(egui::Id::new("broken_references_dialog")).show(ui.ctx(), |ui| {
            ui.heading("Broken References");
            ui.label(format!(
                "Renaming this module to \"{}\" will break {} reference{} into it. Choose how to handle each one:",
                dialog.new_name,
                choices.len(),
                if choices.len() == 1 { "" } else { "s" }
            ));
            egui::Grid::new("broken_references_grid").striped(true).show(ui, |ui| {
                for (index, (site, action)) in choices.iter_mut().enumerate() {
                    ui.label(format!("{} ({})", site.referrer, reference_site_kind_label(&site.kind)));
                    egui::ComboBox::new(("broken_reference_action", index), "")
                        .selected_text(match action {
                            ReferenceAction::Repair => "Repair",
                            ReferenceAction::Remove => "Remove",
                            ReferenceAction::Ignore => "Ignore",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(action, ReferenceAction::Repair, "Repair");
                            ui.selectable_value(action, ReferenceAction::Remove, "Remove");
                            ui.selectable_value(action, ReferenceAction::Ignore, "Ignore");
                        });
                    ui.end_row();
                }
            });
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Confirm")).clicked() {
                    confirmed = true;
                }
                if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            if let Some(dialog) = &mut self.broken_references_dialog {
                dialog.choices = choices;
            }
            self.broken_references_confirmed();
        } else if cancelled {
            self.broken_references_cancelled();
        } else if let Some(dialog) = &mut self.broken_references_dialog {
            dialog.choices = choices;
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

/// Attaches the tree's right-click "Paste" item to `response` — shared by
/// every place a requirement can be pasted into: every module row in the
/// top tree pane that can be a paste target (`render_tree_node`'s two
/// branches below, plus the specially-rendered project root in
/// `render_left_pane`), and the bottom pane's own "requirements" leaf-
/// group header (`render_leaf_group`), which targets whichever module is
/// currently selected. Disabled (not hidden) with nothing on
/// `app.requirement_clipboard`, so right-clicking any of these always
/// shows the same menu shape whether or not a Copy has happened yet.
fn attach_paste_requirement_menu(
    response: &egui::Response,
    app: &mut GuiApp,
    module: Vec<EntryName>,
) {
    response.context_menu(|ui| {
        if ui
            .add_enabled(
                app.requirement_clipboard.is_some(),
                egui::Button::new("Paste"),
            )
            .clicked()
        {
            app.paste_requirement_clicked(module.clone());
            ui.close();
        }
    });
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
    project_validated: bool,
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
        let has_submodules = node
            .children
            .iter()
            .any(|child| child.kind == EntryKind::Module);
        // `CollapsingHeader`'s label renders through button/widget visuals
        // (`ui.visuals().widgets.inactive.fg_stroke`), lighter than a plain
        // `ui.label`'s `ui.visuals().text_color()` — and matches the color
        // `render_leaf`'s `selectable_label`-based rows already use below
        // the separator. Pin the childless-module label (which would
        // otherwise fall back to the darker plain-label color) to that same
        // widget stroke color, so both halves of the tree agree.
        let mut name_text = egui::RichText::new(module_label(node, project_validated))
            .color(ui.visuals().widgets.inactive.fg_stroke.color);
        if is_current {
            name_text = name_text.color(theme_colors::module_current_color(ui.visuals().dark_mode));
        }
        if has_submodules {
            ui.visuals_mut().indent_has_left_vline = false;
            ui.spacing_mut().indent = 18.0;
            let header = egui::CollapsingHeader::new(name_text)
                .default_open(false)
                .open(force_open)
                .show(ui, |ui| {
                    render_module_children(
                        app,
                        ui,
                        &node.children,
                        &this_module_path,
                        force_open,
                        project_validated,
                    );
                });
            attach_paste_requirement_menu(&header.header_response, app, this_module_path.clone());
        } else {
            // No submodules: no expand arrow, but reserve the same
            // horizontal space a CollapsingHeader's toggle button would
            // take (see `show_button_indented` in egui) so labels still
            // line up with sibling modules that do have one.
            ui.spacing_mut().indent = 18.0;
            let size = egui::vec2(ui.spacing().indent, ui.spacing().icon_width);
            let prev_item_spacing = ui.spacing_mut().item_spacing;
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_exact_size(size, egui::Sense::hover());
            ui.spacing_mut().item_spacing = prev_item_spacing;
            // `Sense::click()` — a plain `ui.label` senses only hover, and
            // `Response::context_menu` requires a click-sensing response
            // (see its own doc comment) to detect the right-click at all.
            let response = ui.add(egui::Label::new(name_text).sense(egui::Sense::click()));
            attach_paste_requirement_menu(&response, app, this_module_path.clone());
        }
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
    project_validated: bool,
) {
    for child in children {
        if child.kind == EntryKind::Module {
            render_tree_node(app, ui, child, module_path, force_open, project_validated);
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
    // Requirements are the one kind with a right-click "Paste" of their
    // own (see `attach_paste_requirement_menu`) — a module with zero
    // requirements normally has no "requirements" group at all to right-
    // click, so it's shown anyway (as "requirements (0)") whenever
    // there's actually something on the clipboard to paste, rather than
    // forcing the user out to the top tree pane's own per-module Paste
    // just because this particular module happens to be empty so far.
    let can_paste_here = kind == EntryKind::Requirement && app.requirement_clipboard.is_some();
    if matching.is_empty() && !can_paste_here {
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
    let response = egui::CollapsingHeader::new(header)
        .id_salt((title, module_path))
        .default_open(kind == EntryKind::Requirement)
        .open(display.force_open)
        .show(ui, |ui| {
            for leaf in matching {
                render_leaf(app, ui, leaf, module_path);
            }
        });
    if kind == EntryKind::Requirement {
        attach_paste_requirement_menu(&response.header_response, app, module_path.to_vec());
    }
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
        .map(|child| usize::from(child.kind == kind) + count_kind_recursive(child, kind))
        .sum()
}

/// Renders a requirement or test leaf. Results are never shown in this
/// tree — they're nested under their owning requirement in the data model
/// (see `disk::RequirementOnDisk::results`), but surfaced only in the
/// requirement's own detail panel, not as tree rows.
fn render_leaf(app: &mut GuiApp, ui: &mut egui::Ui, node: &TreeNode, module_path: &[EntryName]) {
    // Both arms need to end up the same type for the one shared
    // `selectable_label` call below — `Atoms` is that common type (a
    // requirement's colored icon + plain name is a 2-`Atom` tuple, every
    // other kind's bare name is a 1-`Atom` string; `.into_atoms()`
    // unifies them, see `egui::IntoAtoms`).
    use egui::IntoAtoms as _;
    // Highlights the leaf currently open in the center pane the same way
    // `render_tree_node` highlights the current module — an accent-colored
    // name, so "this is the active thing" reads consistently whether it's
    // a module or a leaf.
    let is_open = match &app.selection {
        Some(gui_core::EntryPath::Requirement(p)) => {
            node.kind == EntryKind::Requirement && p.modules == module_path && p.name == node.name
        }
        Some(gui_core::EntryPath::Test(p)) => {
            node.kind == EntryKind::Test && p.modules == module_path && p.name == node.name
        }
        _ => false,
    };
    let mut name_text = egui::RichText::new(node.name.as_str());
    if is_open {
        name_text = name_text.color(theme_colors::module_current_color(ui.visuals().dark_mode));
    }
    let content = match node.kind {
        EntryKind::Requirement => {
            let (fg, bg) = crate::theme_colors::status_colors(ui.visuals().dark_mode, node.status);
            let icon = crate::icons::status_icon(node.status);
            (
                egui::RichText::new(icon).color(fg).background_color(bg),
                name_text,
            )
                .into_atoms()
        }
        _ => name_text.into_atoms(),
    };
    let response = ui.selectable_label(false, content);
    let target = LogicalPath {
        modules: module_path.to_vec(),
        name: node.name.clone(),
    };
    if response.clicked() {
        let entry_target = match node.kind {
            EntryKind::Test => gui_core::EntryPath::Test(target.clone()),
            _ => gui_core::EntryPath::Requirement(target.clone()),
        };
        if app.editor_has_unsaved_edits() {
            app.unsaved_form_dialog_opened(PendingNavigation::Select(entry_target));
        } else {
            app.select(entry_target);
        }
    }
    // Copy/Paste/Duplicate/Recreate/Delete are requirement-only affordances
    // for now — tests/results don't have `paste_requirement_clicked`/
    // `duplicate_requirement_clicked`/`recreate_requirement_from_tree_clicked`/
    // `delete_requirement_from_tree_clicked` counterparts to receive one.
    if node.kind == EntryKind::Requirement {
        response.context_menu(|ui| {
            if ui.button("Copy").clicked() {
                app.copy_requirement_clicked(target.clone());
                ui.close();
            }
            if ui.button("Duplicate").clicked() {
                app.duplicate_requirement_clicked(target.clone());
                ui.close();
            }
            if ui.button("Recreate…").clicked() {
                app.recreate_requirement_from_tree_clicked(target.clone());
                ui.close();
            }
            ui.separator();
            if ui.button("Delete…").clicked() {
                app.delete_requirement_from_tree_clicked(target.clone());
                ui.close();
            }
        });
    }
}

/// Walks `root` by `path`, matching only `Module` children at each
/// segment — the `TreeNode` counterpart of `gui-core`'s own
/// `resolve_module` (which walks a `ModuleDraft`, not the simplified
/// read-model tree gui-ui already has in hand each frame). An empty
/// `path` returns `root` itself, the same "empty means project root"
/// convention `selected_module` uses.
pub(crate) fn resolve_tree_module<'a>(
    root: &'a TreeNode,
    path: &[EntryName],
) -> Option<&'a TreeNode> {
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
        "test procedures",
        EntryKind::Test,
        &children,
        &module_path,
        LeafGroupDisplay {
            force_open,
            recursive_total: test_total,
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

/// The "Commit history" collapsible section shown on an existing (not
/// create-mode) Requirement/Test/Result viewer — a log of every commit
/// touching `entry_path`'s own on-disk directory, newest first (see
/// `Command::GetCommitLog`). Reads `GuiApp`'s single commit-log slot
/// (`commit_log_target`/`commit_log`/`commit_log_error` — only one view
/// form is ever open at once, so one slot covers all three callers) rather
/// than taking `&GuiApp` directly, so it can be called while `self.editor`
/// is still mutably borrowed by the caller's own `form`.
///
/// The first time this is expanded for an `entry_path` nothing has been
/// fetched for yet (`commit_log_target != Some(entry_path)`), it writes
/// `entry_path` into `*expand_requested` rather than firing the fetch
/// itself — same deferred-click idiom `render_requirement_form` and its
/// siblings already use throughout (`edit_clicked` et al.), needed here
/// because firing the fetch means calling a `&mut self` method, which
/// can't happen while `self.editor` is already mutably borrowed. The
/// closure only runs on frames where the section is actually expanded, so
/// this naturally fires at most once per newly-opened entry rather than
/// needing to track the header's previous open state.
///
/// Each row's hash is a link — clicking it writes `(entry_path.clone(),
/// commit.hash.clone())` into `*commit_clicked`, the same deferred idiom,
/// for the caller to open the per-commit file-list modal
/// (`GuiApp::commit_files_dialog_opened`) once `self.editor`'s borrow ends.
/// A `commit.hash` empty string is `gui-core`'s synthetic "Uncommitted
/// changes" marker (see `has_uncommitted_changes` in `gui-core::actor`) —
/// there's no real commit for it to open a file list for, so that row is
/// a plain colored label instead of a link and never writes to
/// `commit_clicked`.
fn render_commit_log_section(
    ui: &mut egui::Ui,
    entry_path: &gui_core::EntryPath,
    commit_log_target: Option<&gui_core::EntryPath>,
    commit_log: Option<&[gui_core::CommitInfo]>,
    commit_log_error: Option<&str>,
    dirty: bool,
    expand_requested: &mut Option<gui_core::EntryPath>,
    commit_clicked: &mut Option<(gui_core::EntryPath, String)>,
) {
    egui::CollapsingHeader::new("Commit history")
        .default_open(false)
        .show(ui, |ui| {
            // `dirty` (`GuiApp::dirty`'s own doc comment) reflects whether
            // *any* edit anywhere in the project has reached `gui-core`
            // without yet being saved to disk. A brand new, never-saved
            // entry in particular has no on-disk file at all until Save —
            // the `git`-backed rows below only ever see what's actually on
            // disk (see `has_uncommitted_changes` in `gui-core::actor`), so
            // they have nothing to find for it and, left alone, this
            // section would show only "No commits yet." with no hint that
            // unsaved work exists. Shown unconditionally, ahead of every
            // fetch-dependent branch below, since it's local app state —
            // not something `GetCommitLog` itself reports.
            if dirty {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "The project has unsaved changes not yet written to disk.",
                );
            }
            if commit_log_target != Some(entry_path) {
                *expand_requested = Some(entry_path.clone());
                ui.spinner();
                return;
            }
            if let Some(error) = commit_log_error {
                ui.colored_label(egui::Color32::RED, error);
                return;
            }
            let Some(commits) = commit_log else {
                // Fetch already under way (`commit_log_target` matches,
                // but neither a result nor an error has landed yet).
                ui.spinner();
                return;
            };
            if commits.is_empty() {
                ui.label("No commits yet.");
                return;
            }
            egui::Grid::new("commit_log_grid").striped(true).show(ui, |ui| {
                for commit in commits {
                    if commit.hash.is_empty() {
                        ui.colored_label(egui::Color32::YELLOW, &commit.subject);
                        ui.label("");
                        ui.label("");
                        ui.end_row();
                        continue;
                    }
                    if ui.link(&commit.hash[..commit.hash.len().min(8)]).clicked() {
                        *commit_clicked = Some((entry_path.clone(), commit.hash.clone()));
                    }
                    ui.label(&commit.date);
                    ui.label(&commit.subject);
                    ui.end_row();
                }
            });
        });
}

/// The "Push" confirm-state preview of `dialog.unpushed_commits` — what a
/// click of the dialog's own "Push" button would actually send, fetched by
/// `GuiApp::push_button_clicked` alongside opening the dialog. A `RED`
/// error label for a failed fetch and a spinner while it's in flight, same
/// convention `render_commit_log_section` uses; an empty (but successfully
/// fetched) list gets its own `YELLOW` warning rather than the plain "No
/// commits yet." label that section uses, since "nothing to push" here is
/// something the user is about to act on, not just informational history.
fn render_unpushed_commits_preview(ui: &mut egui::Ui, dialog: &PushDialogState) {
    if dialog.unpushed_loading {
        ui.spinner();
        return;
    }
    if let Some(error) = &dialog.unpushed_error {
        ui.colored_label(egui::Color32::RED, format!("Unable to check for unpushed commits: {error}"));
        return;
    }
    let Some(commits) = &dialog.unpushed_commits else {
        return;
    };
    if commits.is_empty() {
        ui.colored_label(egui::Color32::YELLOW, "No commits to push.");
        return;
    }
    ui.label(format!(
        "{} commit{} to push:",
        commits.len(),
        if commits.len() == 1 { "" } else { "s" }
    ));
    egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
        egui::Grid::new("push_unpushed_commits_grid").striped(true).show(ui, |ui| {
            for commit in commits {
                ui.label(&commit.hash[..commit.hash.len().min(8)]);
                ui.label(&commit.date);
                ui.label(&commit.subject);
                ui.end_row();
            }
        });
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
/// Same variant switch as `render_dependency_kind_picker`, as a `ComboBox`
/// instead of radio buttons — used for an existing dependency's own row,
/// where a compact single-line control fits the row layout better; the
/// "Add dependency" composer keeps the radio-button picker since it isn't
/// squeezed into a row.
fn render_dependency_kind_dropdown(
    ui: &mut egui::Ui,
    id_source: usize,
    dep: &mut DependencyDraft,
) -> bool {
    let mut changed = false;
    egui::ComboBox::new(("dependency_kind", id_source), "")
        .selected_text(match dep {
            DependencyDraft::LocalRequirement { .. } => "Local",
            DependencyDraft::Remote { .. } => "Remote",
            DependencyDraft::Submodules => "All submodules",
            DependencyDraft::Submodule { .. } => "Submodule",
        })
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(
                    matches!(dep, DependencyDraft::LocalRequirement { .. }),
                    "Local",
                )
                .clicked()
                && !matches!(dep, DependencyDraft::LocalRequirement { .. })
            {
                *dep = DependencyDraft::LocalRequirement {
                    path: String::new(),
                    commit: String::new(),
                };
                changed = true;
            }
            if ui
                .selectable_label(matches!(dep, DependencyDraft::Remote { .. }), "Remote")
                .clicked()
                && !matches!(dep, DependencyDraft::Remote { .. })
            {
                *dep = DependencyDraft::Remote {
                    url: String::new(),
                    path: String::new(),
                    commit: String::new(),
                };
                changed = true;
            }
            if ui
                .selectable_label(matches!(dep, DependencyDraft::Submodules), "All submodules")
                .clicked()
                && !matches!(dep, DependencyDraft::Submodules)
            {
                *dep = DependencyDraft::Submodules;
                changed = true;
            }
            if ui
                .selectable_label(
                    matches!(dep, DependencyDraft::Submodule { .. }),
                    "Submodule",
                )
                .clicked()
                && !matches!(dep, DependencyDraft::Submodule { .. })
            {
                *dep = DependencyDraft::Submodule {
                    name: String::new(),
                };
                changed = true;
            }
        });
    changed
}

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
        .radio(matches!(dep, DependencyDraft::Submodules), "All submodules")
        .clicked()
    {
        *dep = DependencyDraft::Submodules;
        return true;
    }
    if ui
        .radio(
            matches!(dep, DependencyDraft::Submodule { .. }),
            "Submodule",
        )
        .clicked()
    {
        *dep = DependencyDraft::Submodule {
            name: String::new(),
        };
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
    current_module: &[EntryName],
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
        DependencyDraft::Submodule { name } => {
            let mut changed = false;
            let submodules = tree
                .map(|tree| direct_submodule_names(tree, current_module))
                .unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label("Submodule:");
                if submodules.is_empty() {
                    // No tree loaded yet, or this module has no
                    // submodules — fall back to a hand-typed field rather
                    // than an empty, unusable dropdown (same graceful
                    // degrade `LocalRequirement`'s "Pick…" button follows
                    // when `tree` is `None`).
                    changed |= ui.text_edit_singleline(name).changed();
                } else {
                    egui::ComboBox::new("submodule_dependency_name", "")
                        .selected_text(if name.is_empty() {
                            "(choose a submodule)"
                        } else {
                            name.as_str()
                        })
                        .show_ui(ui, |ui| {
                            for candidate in &submodules {
                                if ui.selectable_label(name == candidate, candidate).clicked()
                                    && name != candidate
                                {
                                    *name = candidate.clone();
                                    changed = true;
                                }
                            }
                        });
                }
            });
            (changed, None, false)
        }
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
                        absolute_reference_path(target, leaf_kind_segment(LeafKind::Test))
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
        EntryKind::Requirement | EntryKind::Test => {
            let leaf_kind = if node.kind == EntryKind::Requirement {
                LeafKind::Requirement
            } else {
                LeafKind::Test
            };
            let target = LogicalPath {
                modules: module_path.to_vec(),
                name: node.name.clone(),
            };
            absolute_reference_path(&target, leaf_kind_segment(leaf_kind))
                .to_lowercase()
                .contains(&filter)
        }
        // A result has no reference-path identity of its own to search —
        // see `leaf_kind_segment`'s doc comment — and nothing currently
        // renders a filterable "results" group (see `render_leaf_group`'s
        // own doc comment on why that name still appears there). Until one
        // exists, err on the side of not hiding it rather than panicking.
        EntryKind::Result => true,
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
            requirement_count: 0,
            requirements_met: 0,
            children: Vec::new(),
        }
    }

    fn module(name_str: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            name: name(name_str),
            kind: EntryKind::Module,
            status: EntryStatus::Unvalidated,
            requirement_count: 0,
            requirements_met: 0,
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
            validated: false,
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
            validated: false,
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
            validated: false,
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
            validated: false,
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
    fn scoped_leaf_paths_with_all_scope_matches_flatten_leaf_paths() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![
                    module("setup", vec![leaf(EntryKind::Requirement, "nested")]),
                    leaf(EntryKind::Requirement, "root_level"),
                ],
            ),
            can_undo: false,
            can_redo: false,
            validated: false,
        };

        assert_eq!(
            scoped_leaf_paths(&tree, EntryKind::Requirement, PathPickerScope::All, &[]),
            flatten_leaf_paths(&tree, EntryKind::Requirement)
        );
    }

    #[test]
    fn scoped_leaf_paths_this_module_excludes_the_parent_and_submodules() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![
                    module(
                        "setup",
                        vec![
                            leaf(EntryKind::Requirement, "direct"),
                            module("deeper", vec![leaf(EntryKind::Requirement, "nested")]),
                        ],
                    ),
                    leaf(EntryKind::Requirement, "root_level"),
                ],
            ),
            can_undo: false,
            can_redo: false,
            validated: false,
        };

        let requirements = scoped_leaf_paths(
            &tree,
            EntryKind::Requirement,
            PathPickerScope::ThisModule,
            &[name("setup")],
        );

        assert_eq!(
            requirements,
            vec![LogicalPath {
                modules: vec![name("setup")],
                name: name("direct"),
            }]
        );
    }

    #[test]
    fn scoped_leaf_paths_submodules_excludes_the_module_itself_and_unrelated_modules() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![
                    module(
                        "setup",
                        vec![
                            leaf(EntryKind::Requirement, "direct"),
                            module("deeper", vec![leaf(EntryKind::Requirement, "nested")]),
                        ],
                    ),
                    leaf(EntryKind::Requirement, "root_level"),
                ],
            ),
            can_undo: false,
            can_redo: false,
            validated: false,
        };

        let requirements = scoped_leaf_paths(
            &tree,
            EntryKind::Requirement,
            PathPickerScope::Submodules,
            &[name("setup")],
        );

        assert_eq!(
            requirements,
            vec![LogicalPath {
                modules: vec![name("setup"), name("deeper")],
                name: name("nested"),
            }]
        );
    }

    #[test]
    fn scoped_leaf_paths_with_an_empty_owning_module_treats_this_module_as_the_project_root() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![
                    module("setup", vec![leaf(EntryKind::Requirement, "nested")]),
                    leaf(EntryKind::Requirement, "root_level"),
                ],
            ),
            can_undo: false,
            can_redo: false,
            validated: false,
        };

        assert_eq!(
            scoped_leaf_paths(
                &tree,
                EntryKind::Requirement,
                PathPickerScope::ThisModule,
                &[]
            ),
            vec![LogicalPath::root(name("root_level"))]
        );
        assert_eq!(
            scoped_leaf_paths(
                &tree,
                EntryKind::Requirement,
                PathPickerScope::Submodules,
                &[]
            ),
            vec![LogicalPath {
                modules: vec![name("setup")],
                name: name("nested"),
            }]
        );
    }

    #[test]
    fn direct_submodule_names_lists_only_module_children_of_the_project_root() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![
                    module("alpha", Vec::new()),
                    module("beta", Vec::new()),
                    leaf(EntryKind::Requirement, "root_level"),
                ],
            ),
            can_undo: false,
            can_redo: false,
            validated: false,
        };

        assert_eq!(
            direct_submodule_names(&tree, &[]),
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    #[test]
    fn direct_submodule_names_walks_down_to_a_nested_module() {
        let tree = TreeSnapshot {
            root: module(
                "Capstone",
                vec![module("setup", vec![module("nested", Vec::new())])],
            ),
            can_undo: false,
            can_redo: false,
            validated: false,
        };

        assert_eq!(
            direct_submodule_names(&tree, &[name("setup")]),
            vec!["nested".to_string()]
        );
    }

    #[test]
    fn direct_submodule_names_is_empty_for_an_unknown_module_path() {
        let tree = TreeSnapshot {
            root: module("Capstone", vec![module("alpha", Vec::new())]),
            can_undo: false,
            can_redo: false,
            validated: false,
        };

        assert!(direct_submodule_names(&tree, &[name("nonexistent")]).is_empty());
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
