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
        (false, EntryStatus::Met) => (Color32::from_rgb(0x1a, 0x7f, 0x37), Color32::from_rgb(0xda, 0xfb, 0xe1)),
        (false, EntryStatus::Unmet) => (Color32::from_rgb(0xcf, 0x22, 0x2e), Color32::from_rgb(0xff, 0xeb, 0xe9)),
        (false, EntryStatus::Unvalidated) => (Color32::from_rgb(0x59, 0x63, 0x6e), Color32::from_rgb(0xf0, 0xf1, 0xf3)),
        (true, EntryStatus::Met) => (Color32::from_rgb(0x7e, 0xe7, 0x87), Color32::from_rgb(0x0f, 0x2a, 0x1a)),
        (true, EntryStatus::Unmet) => (Color32::from_rgb(0xff, 0x9a, 0x92), Color32::from_rgb(0x3a, 0x11, 0x14)),
        (true, EntryStatus::Unvalidated) => (Color32::from_rgb(0x9a, 0xa4, 0xaf), Color32::from_rgb(0x2a, 0x2d, 0x31)),
    }
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
