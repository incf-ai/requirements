//! Registers the Phosphor icon font gui-ui's own status/action icons draw
//! from — see `icons.rs`. Split out from `main.rs` only because it's a
//! distinct, testable-in-isolation concern; there's no other reason for a
//! whole module.

/// Registers the Phosphor icon font as a fallback in egui's Proportional
/// family, right after the default "Ubuntu-Light" text font and ahead of
/// the emoji fallbacks — same insertion point `egui_phosphor::add_to_fonts`
/// itself uses. Reimplemented by hand rather than calling that function
/// directly: `egui-phosphor` (crates.io) pins `egui = "0.35"`, incompatible
/// with the `0.36.1` this workspace pins, so its own `add_to_fonts`/
/// `Variant::font_data()` take/return *its* `egui` types, not ours — a
/// hard compile error if called with our real `egui::FontDefinitions`.
/// `Variant::font_bytes()` returns a plain `&'static [u8]`, though, with no
/// egui-type coupling at all, so that's the one thing of theirs this
/// actually calls.
pub fn install_icon_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "phosphor".to_owned(),
        egui::FontData::from_static(egui_phosphor::Variant::Regular.font_bytes()).into(),
    );
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .expect("egui's default FontDefinitions always has a Proportional family")
        .insert(1, "phosphor".to_owned());
    ctx.set_fonts(fonts);
}
