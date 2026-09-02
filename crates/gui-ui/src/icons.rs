//! Semantic name -> Phosphor glyph, one place so a status/action's icon can
//! change without hunting through `view.rs` for the literal glyph string.
//! Grouped by call site (status, toolbar, menu, the handful of pre-existing
//! ad hoc glyphs), not alphabetized — matches how they're actually used.

use gui_core::EntryStatus;

/// The tree's own per-requirement status icon — see `theme_colors::status_colors`
/// for the color half of the same job. Renamed from the old `status_glyph`
/// (same signature) now that the glyphs themselves come from Phosphor
/// rather than whatever egui's bundled default fonts happened to cover.
pub fn status_icon(status: EntryStatus) -> &'static str {
    match status {
        EntryStatus::Met => egui_phosphor::regular::CHECK_CIRCLE,
        EntryStatus::Unmet => egui_phosphor::regular::X_CIRCLE,
        EntryStatus::Unvalidated => egui_phosphor::regular::MINUS_CIRCLE,
    }
}

// Toolbar (`render_toolbar`).
pub const SAVE: &str = egui_phosphor::regular::FLOPPY_DISK;
pub const VALIDATE: &str = egui_phosphor::regular::SEAL_CHECK;
pub const UNDO: &str = egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE;
pub const REDO: &str = egui_phosphor::regular::ARROW_CLOCKWISE;
pub const BACK: &str = egui_phosphor::regular::ARROW_LEFT;
pub const FORWARD: &str = egui_phosphor::regular::ARROW_RIGHT;
pub const EXIT: &str = egui_phosphor::regular::SIGN_OUT;
pub const NEW_REQUIREMENT: &str = egui_phosphor::regular::LIST_CHECKS;
pub const NEW_TEST: &str = egui_phosphor::regular::TEST_TUBE;
pub const NEW_RESULT: &str = egui_phosphor::regular::CHART_BAR;
pub const NEW_MODULE: &str = egui_phosphor::regular::FOLDER_PLUS;
pub const ATTACHMENTS: &str = egui_phosphor::regular::PAPERCLIP;

// Menu bar (`render_menu_bar`'s File menu).
pub const NEW_PROJECT: &str = egui_phosphor::regular::FILE_PLUS;
pub const OPEN_PROJECT: &str = egui_phosphor::regular::FOLDER_OPEN;
pub const SAVE_AS: &str = egui_phosphor::regular::FLOPPY_DISK_BACK;

// Requirement viewer.
/// The "Update Stale References" button, next to "Edit" — distinct from
/// `UNDO`'s single counter-clockwise arrow, a circular two-arrow "sync"
/// glyph reads more like "bring this up to date" than "undo".
pub const UPDATE_STALE_REFERENCES: &str = egui_phosphor::regular::ARROWS_CLOCKWISE;

// Pre-existing ad hoc glyphs, migrated to the same icon set — see each
// call site in `view.rs` for why these aren't colored like `status_icon`
// (none of them are a Met/Unmet-style status).
pub const MODULE_CURRENT: &str = egui_phosphor::regular::FOLDER_NOTCH_OPEN;
pub const MODULE_NOT_CURRENT: &str = egui_phosphor::regular::FOLDER_NOTCH;
pub const UNSAVED: &str = egui_phosphor::regular::CIRCLE;

// Debug panel toggle (`render_menu_bar`'s far-right corner).
pub const DEBUG_PANEL_OPEN: &str = egui_phosphor::regular::CARET_UP;
pub const DEBUG_PANEL_CLOSED: &str = egui_phosphor::regular::CARET_DOWN;
