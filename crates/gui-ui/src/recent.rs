//! `recent.ron` — the "recently opened projects" list shown in the File
//! menu's "Open Recent" submenu. See `README.md`'s "Configuration"
//! section and "Recently opened projects."

use std::path::{Path, PathBuf};

use ron::extensions::Extensions;
use serde::{Deserialize, Serialize};

/// How many entries `record` keeps — unbounded growth over a long-lived
/// machine/session isn't useful in a menu meant to be glanced at, not
/// scrolled through.
const MAX_ENTRIES: usize = 10;

/// Most-recent-first, deduplicated. Recording a path already present
/// moves it to the front rather than duplicating it — see `record`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RecentProjects {
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
#[error("failed to parse recent.ron: {0}")]
pub struct LoadError(#[from] ron::de::SpannedError);

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("failed to serialize recent.ron: {0}")]
    Serialize(#[from] ron::Error),
    #[error("failed to write recent.ron: {0}")]
    Io(#[from] std::io::Error),
}

impl RecentProjects {
    /// A missing file is not an error — falls back to `Default` (an
    /// empty list). Same convention as `GuiConfig::load`, see that
    /// function's own doc comment for why.
    pub fn load(path: &Path) -> (RecentProjects, Option<LoadError>) {
        let contents = match std::fs::read_to_string(path) {
            Err(_) => return (RecentProjects::default(), None),
            Ok(contents) => contents,
        };

        match ron_options().from_str::<RecentProjects>(&contents) {
            Ok(recent) => (recent, None),
            Err(err) => (RecentProjects::default(), Some(LoadError(err))),
        }
    }

    /// Writes the whole list back to `path` — same shared `ron_options()`
    /// symmetry `GuiConfig::save` needs (see that function's own doc
    /// comment on the `ExpectedStructName` bug this avoids).
    pub fn save(&self, path: &Path) -> Result<(), SaveError> {
        let contents = ron_options().to_string_pretty(self, ron::ser::PrettyConfig::default())?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Moves `path` to the front, deduplicating if it was already
    /// present, then truncates to `MAX_ENTRIES` — called on every
    /// successful `LoadProject`/`SaveAs`/`Save` (see `GuiApp::
    /// record_recent_project`), so a project already in the list bumps
    /// back to the top rather than growing a second entry.
    pub fn record(&mut self, path: PathBuf) {
        self.paths.retain(|p| p != &path);
        self.paths.insert(0, path);
        self.paths.truncate(MAX_ENTRIES);
    }
}

fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(Extensions::all())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_missing_file_falls_back_to_an_empty_list_with_no_error() {
        let (recent, error) = RecentProjects::load(Path::new("/nonexistent/recent.ron"));
        assert!(recent.paths.is_empty());
        assert!(error.is_none());
    }

    #[test]
    fn malformed_ron_falls_back_to_an_empty_list_and_reports_the_error() {
        let dir = std::env::temp_dir().join(format!("gui-ui-recent-test-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recent.ron");
        std::fs::write(&path, "not valid ron (").unwrap();

        let (recent, error) = RecentProjects::load(&path);
        assert!(recent.paths.is_empty());
        assert!(error.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("gui-ui-recent-test-round-trip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recent.ron");

        let recent = RecentProjects {
            paths: vec![PathBuf::from("/a/project"), PathBuf::from("/b/project")],
        };
        recent.save(&path).unwrap();

        let (loaded, error) = RecentProjects::load(&path);
        assert!(error.is_none());
        assert_eq!(loaded.paths, vec![PathBuf::from("/a/project"), PathBuf::from("/b/project")]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_to_an_unwritable_path_reports_an_io_error() {
        let err = RecentProjects::default().save(Path::new("/nonexistent-directory/recent.ron")).unwrap_err();
        assert!(matches!(err, SaveError::Io(_)));
    }

    #[test]
    fn recording_a_new_path_adds_it_to_the_front() {
        let mut recent = RecentProjects {
            paths: vec![PathBuf::from("/a")],
        };
        recent.record(PathBuf::from("/b"));
        assert_eq!(recent.paths, vec![PathBuf::from("/b"), PathBuf::from("/a")]);
    }

    #[test]
    fn recording_an_already_present_path_moves_it_to_the_front_instead_of_duplicating() {
        let mut recent = RecentProjects {
            paths: vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")],
        };
        recent.record(PathBuf::from("/b"));
        assert_eq!(recent.paths, vec![PathBuf::from("/b"), PathBuf::from("/a"), PathBuf::from("/c")]);
    }

    #[test]
    fn recording_past_the_cap_drops_the_oldest_entry() {
        // Index 0 is most-recent-first, so `/project-0` is the freshest
        // of the seed entries and `/project-9` (`MAX_ENTRIES - 1`) is the
        // oldest — the one `truncate` should drop once a new entry pushes
        // the list past the cap.
        let mut recent = RecentProjects {
            paths: (0..MAX_ENTRIES).map(|i| PathBuf::from(format!("/project-{i}"))).collect(),
        };
        recent.record(PathBuf::from("/newest"));
        assert_eq!(recent.paths.len(), MAX_ENTRIES);
        assert_eq!(recent.paths[0], PathBuf::from("/newest"));
        assert!(recent.paths.contains(&PathBuf::from("/project-0")));
        assert!(!recent.paths.contains(&PathBuf::from("/project-9")));
    }
}
