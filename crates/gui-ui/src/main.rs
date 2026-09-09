fn main() {
    // TODO: real config path (platform config dir vs. project-adjacent —
    // see README's Configuration section, "Open"). Project-relative
    // placeholder for now.
    let config_path = std::path::PathBuf::from("gui-config.ron");
    let (config, config_error) = gui_ui::GuiConfig::load(&config_path);
    if let Some(err) = config_error {
        eprintln!("warning: {err}");
    }

    let recent_path = std::path::PathBuf::from("recent.ron");
    let (recent, recent_error) = gui_ui::RecentProjects::load(&recent_path);
    if let Some(err) = recent_error {
        eprintln!("warning: {err}");
    }

    let core = match gui_core::CoreHandle::start() {
        Ok(core) => core,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let app = gui_ui::GuiApp::new(core, config, config_path, recent, recent_path);

    if let Err(err) = eframe::run_native(
        "IncreRMS",
        eframe::NativeOptions::default(),
        Box::new(|cc| {
            gui_ui::install_icon_font(&cc.egui_ctx);
            // Disable egui's animations (fades, collapsing, and the
            // grow-to-content-size animation new windows/modals play on
            // their first frame) — modals like the commit/diff dialogs
            // otherwise visibly grow to full screen height over ~0.25s.
            cc.egui_ctx
                .all_styles_mut(|style| style.animation_time = 0.0);
            Ok(Box::new(app))
        }),
    ) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
