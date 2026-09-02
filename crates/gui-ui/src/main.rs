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

    let core = gui_core::CoreHandle::start();
    let app = gui_ui::GuiApp::new(core, config, config_path, recent, recent_path);

    if let Err(err) = eframe::run_native(
        "IncRMS",
        eframe::NativeOptions::default(),
        Box::new(|cc| {
            gui_ui::install_icon_font(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    ) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
