//! Theme-aware (foreground, background) pairs for status chips — kept
//! separate from `icons.rs` since this is about color, not glyph choice,
//! and the two vary independently (a status keeps its icon across themes,
//! only the coloring changes).

use egui::Color32;
use gui_core::EntryStatus;

/// `(foreground, background)` for a status chip, tuned per theme so the
/// pale background stays legible against `egui::Visuals::dark_mode`'s own
/// panel color rather than washing out or clashing. GitHub Primer-style
/// semantic tokens (pale tint background + saturated-but-readable
/// foreground) as a starting palette — not final; visually confirm in both
/// themes via `cargo run -p gui-ui --bin gui` before treating these as
/// settled.
pub fn status_colors(dark_mode: bool, status: EntryStatus) -> (Color32, Color32) {
    match (dark_mode, status) {
        (false, EntryStatus::Met) => (
            Color32::from_rgb(0x1a, 0x7f, 0x37),
            Color32::from_rgb(0xda, 0xfb, 0xe1),
        ),
        (false, EntryStatus::Unmet) => (
            Color32::from_rgb(0xcf, 0x22, 0x2e),
            Color32::from_rgb(0xff, 0xeb, 0xe9),
        ),
        (false, EntryStatus::Unvalidated) => (
            Color32::from_rgb(0x59, 0x63, 0x6e),
            Color32::from_rgb(0xf0, 0xf1, 0xf3),
        ),
        (true, EntryStatus::Met) => (
            Color32::from_rgb(0x7e, 0xe7, 0x87),
            Color32::from_rgb(0x0f, 0x2a, 0x1a),
        ),
        (true, EntryStatus::Unmet) => (
            Color32::from_rgb(0xff, 0x9a, 0x92),
            Color32::from_rgb(0x3a, 0x11, 0x14),
        ),
        (true, EntryStatus::Unvalidated) => (
            Color32::from_rgb(0x9a, 0xa4, 0xaf),
            Color32::from_rgb(0x2a, 0x2d, 0x31),
        ),
    }
}

/// Which part of a unified diff a line belongs to — see
/// `classify_diff_line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Addition,
    Deletion,
    /// Everything else: context lines, and the `---`/`+++`/`@@`/`diff
    /// --git` header lines around them — left in the theme's ordinary
    /// text color rather than tinted, so only the lines a reviewer
    /// actually cares about stand out.
    Context,
}

/// Classifies one line of a unified diff (as `syscalls::Git::diff`
/// returns it) by its leading marker.
pub fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with("+++") || line.starts_with("---") {
        DiffLineKind::Context
    } else if line.starts_with('+') {
        DiffLineKind::Addition
    } else if line.starts_with('-') {
        DiffLineKind::Deletion
    } else {
        DiffLineKind::Context
    }
}

/// `(foreground, background)` for a colored diff line, or `None` for
/// `DiffLineKind::Context` (left uncolored). Same GitHub Primer-style
/// pale-background/saturated-foreground pairing as `status_colors`'
/// `Met`/`Unmet` — "added" and "removed" are the same green/red semantics.
pub fn diff_line_colors(dark_mode: bool, kind: DiffLineKind) -> Option<(Color32, Color32)> {
    match (dark_mode, kind) {
        (_, DiffLineKind::Context) => None,
        (false, DiffLineKind::Addition) => Some((
            Color32::from_rgb(0x1a, 0x7f, 0x37),
            Color32::from_rgb(0xda, 0xfb, 0xe1),
        )),
        (false, DiffLineKind::Deletion) => Some((
            Color32::from_rgb(0xcf, 0x22, 0x2e),
            Color32::from_rgb(0xff, 0xeb, 0xe9),
        )),
        (true, DiffLineKind::Addition) => Some((
            Color32::from_rgb(0x7e, 0xe7, 0x87),
            Color32::from_rgb(0x0f, 0x2a, 0x1a),
        )),
        (true, DiffLineKind::Deletion) => Some((
            Color32::from_rgb(0xff, 0x9a, 0x92),
            Color32::from_rgb(0x3a, 0x11, 0x14),
        )),
    }
}

/// Underline color for a misspelled word in a spellchecked prose field
/// (see `view.rs`'s `spellcheck_text_layouter`) — reuses `status_colors`'
/// `Unmet` foreground rather than inventing a new color, so a misspelling
/// reads with the same "needs attention" semantic as a failing status
/// chip.
pub fn misspelling_underline_color(dark_mode: bool) -> Color32 {
    status_colors(dark_mode, EntryStatus::Unmet).0
}

/// Foreground for the "set as current module" glyph (`icons::MODULE_CURRENT`)
/// when it *is* the current module — the not-current glyph keeps egui's
/// default text color. Same GitHub Primer-derived blue used for both
/// themes' "accent"/link color, so it reads as "selected" rather than as
/// another status color.
pub fn module_current_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(0x6c, 0xb6, 0xff)
    } else {
        Color32::from_rgb(0x09, 0x69, 0xda)
    }
}
