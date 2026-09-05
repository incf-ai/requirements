//! `gui-config.ron`. See `README.md`'s "Configuration" section.

use std::path::Path;
use std::time::Duration;

use ron::extensions::Extensions;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub save_on_exit_timeout: Duration,
    /// The UI's zoom level, as a percentage — `100` is egui's own
    /// default `zoom_factor` of `1.0`. Kept as a whole-number percentage
    /// (not the raw `f32` factor) since that's what's actually shown and
    /// stepped in the status bar's zoom controls (`GuiApp::zoom_in_clicked`/
    /// `zoom_out_clicked`, both in 10-point steps) — converting to
    /// `egui::Context::set_zoom_factor`'s `f32` only happens at the one
    /// point that needs it, in `GuiApp::ui`.
    pub zoom_percent: u32,
    /// The status bar's theme selector — see `ThemeChoice`'s own doc
    /// comment on why this is `gui-ui`'s own type rather than
    /// `egui::ThemePreference` directly.
    pub theme: ThemeChoice,
}

impl Default for GuiConfig {
    fn default() -> GuiConfig {
        GuiConfig {
            save_on_exit_timeout: Duration::from_secs(15),
            zoom_percent: 100,
            theme: ThemeChoice::default(),
        }
    }
}

/// Light, Dark, or follow the OS ("System") — the status bar's theme
/// selector. A local copy of `egui::ThemePreference`'s three variants
/// rather than that type itself: persisting it directly would need
/// enabling egui's `serde` feature crate-wide (pulling in `accesskit/serde`
/// and `epaint/serde` too) just for this one config field, where a small
/// mirrored enum plus a one-line `From` conversion (see below) costs
/// nothing extra — `gui-ui` already depends on `serde` for `GuiConfig`
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeChoice {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemeChoice {
    /// Every choice, in the order the status bar's selector lists them.
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System];

    /// The label the status bar's selector shows for this choice.
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
            ThemeChoice::System => "System",
        }
    }
}

impl From<ThemeChoice> for egui::ThemePreference {
    fn from(choice: ThemeChoice) -> egui::ThemePreference {
        match choice {
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::System => egui::ThemePreference::System,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("failed to parse gui-config.ron: {0}")]
pub struct LoadError(#[from] ron::de::SpannedError);

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("failed to serialize gui-config.ron: {0}")]
    Serialize(#[from] ron::Error),
    #[error("failed to write gui-config.ron: {0}")]
    Io(#[from] std::io::Error),
}

impl GuiConfig {
    /// A missing file is not an error — falls back to `Default`. A
    /// malformed file returns `Default` alongside the error, so the caller
    /// can still start the app and surface the error as a status-bar
    /// warning rather than refusing to run. See README's "Configuration"
    /// section — deliberately every RON extension enabled, not `disk`'s
    /// narrower hand-picked set, since this file has no round-trip/
    /// authoring-consistency concerns riding on it.
    pub fn load(path: &Path) -> (GuiConfig, Option<LoadError>) {
        let contents = match std::fs::read_to_string(path) {
            // Missing (or unreadable) file: not an error, just defaults.
            // `serde(default)` alone can't cover a whole-file miss, only
            // missing fields within an existing file.
            Err(_) => return (GuiConfig::default(), None),
            Ok(contents) => contents,
        };

        match ron_options().from_str::<GuiConfig>(&contents) {
            Ok(config) => (config, None),
            Err(err) => (GuiConfig::default(), Some(LoadError(err))),
        }
    }

    /// Writes the whole config back to `path` — currently only used to
    /// persist a changed `zoom_percent` (see `GuiApp::zoom_in_clicked`/
    /// `zoom_out_clicked`), but saves every field, not just that one, so
    /// nothing else the user has hand-edited in `gui-config.ron` gets
    /// silently dropped by an unrelated zoom click. A real synchronous
    /// filesystem write directly from `gui-ui` — same narrow, deliberate
    /// exception to "never do its own filesystem IO" as `GuiConfig::load`
    /// itself already is (see README's "Never block the render thread"):
    /// this is `gui-ui`'s own local settings file, entirely outside
    /// `gui-core`'s project-data path, and the write is tiny and bounded.
    ///
    /// Serializes through the same `ron_options()` (`Extensions::all()`)
    /// `load` parses with, not the bare `ron::ser::to_string_pretty` free
    /// function — the free function omits the struct name RON normally
    /// makes optional, but `Extensions::all()` includes `EXPLICIT_STRUCT_
    /// NAMES`, which makes `load` *require* it; serializing without the
    /// matching options produced a file `load` itself couldn't parse back
    /// (`ExpectedStructName`), caught by `save_then_load_round_trips_
    /// every_field` below.
    pub fn save(&self, path: &Path) -> Result<(), SaveError> {
        let contents = ron_options().to_string_pretty(self, ron::ser::PrettyConfig::default())?;
        std::fs::write(path, contents)?;
        Ok(())
    }
}

fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(Extensions::all())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_missing_file_falls_back_to_default_with_no_error() {
        let (config, error) = GuiConfig::load(Path::new("/nonexistent/gui-config.ron"));
        assert_eq!(config.save_on_exit_timeout, Duration::from_secs(15));
        assert_eq!(config.zoom_percent, 100);
        assert_eq!(config.theme, ThemeChoice::System);
        assert!(error.is_none());
    }

    #[test]
    fn an_overridden_field_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "gui-ui-config-test-override-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui-config.ron");
        std::fs::write(
            &path,
            "GuiConfig(save_on_exit_timeout: Duration(secs: 5, nanos: 0))",
        )
        .unwrap();

        let (config, error) = GuiConfig::load(&path);
        assert!(error.is_none());
        assert_eq!(config.save_on_exit_timeout, Duration::from_secs(5));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_falls_back_to_default_fields() {
        let dir =
            std::env::temp_dir().join(format!("gui-ui-config-test-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui-config.ron");
        std::fs::write(&path, "GuiConfig()").unwrap();

        let (config, error) = GuiConfig::load(&path);
        assert!(error.is_none());
        assert_eq!(config.save_on_exit_timeout, Duration::from_secs(15));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_ron_falls_back_to_default_and_reports_the_error() {
        let dir = std::env::temp_dir().join(format!(
            "gui-ui-config-test-malformed-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui-config.ron");
        std::fs::write(&path, "not valid ron (").unwrap();

        let (config, error) = GuiConfig::load(&path);
        assert_eq!(config.save_on_exit_timeout, Duration::from_secs(15));
        assert!(error.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips_every_field() {
        let dir = std::env::temp_dir().join(format!(
            "gui-ui-config-test-save-round-trip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui-config.ron");

        let config = GuiConfig {
            save_on_exit_timeout: Duration::from_secs(30),
            zoom_percent: 150,
            theme: ThemeChoice::Dark,
        };
        config.save(&path).unwrap();

        let (loaded, error) = GuiConfig::load(&path);
        assert!(error.is_none());
        assert_eq!(loaded.save_on_exit_timeout, Duration::from_secs(30));
        assert_eq!(loaded.zoom_percent, 150);
        assert_eq!(loaded.theme, ThemeChoice::Dark);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_overwrites_an_existing_file_rather_than_merging() {
        let dir = std::env::temp_dir().join(format!(
            "gui-ui-config-test-save-overwrites-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui-config.ron");
        std::fs::write(
            &path,
            "GuiConfig(save_on_exit_timeout: Duration(secs: 5, nanos: 0), zoom_percent: 80)",
        )
        .unwrap();

        GuiConfig::default().save(&path).unwrap();

        let (loaded, error) = GuiConfig::load(&path);
        assert!(error.is_none());
        assert_eq!(loaded.save_on_exit_timeout, Duration::from_secs(15));
        assert_eq!(loaded.zoom_percent, 100);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_to_an_unwritable_path_reports_an_io_error() {
        let err = GuiConfig::default()
            .save(Path::new("/nonexistent-directory/gui-config.ron"))
            .unwrap_err();
        assert!(matches!(err, SaveError::Io(_)));
    }
}
