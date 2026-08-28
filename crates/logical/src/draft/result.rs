use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use disk::{AttachmentReferenceKind, ReferencePath, StatusV1};

use crate::pool::{AddPoolFileError, add_pool_file};

/// A result, freely editable. `requirement_path`/`requirement_commit` and
/// `test_path`/`test_commit` mirror `disk::ResultsV1` exactly (see
/// `crates/logical/README.md`'s "Requirement-met semantics") — `logical`
/// doesn't resolve or check these until `validate()`.
#[derive(Debug, Clone)]
pub struct ResultDraft {
    pub title: String,
    pub requirement_path: ReferencePath,
    pub requirement_commit: String,
    pub test_path: ReferencePath,
    pub test_commit: String,
    pub status: StatusV1,
    /// Files physically local to this result's own `attachments/`.
    pub attachments: BTreeSet<PathBuf>,
    /// This result's `attachment`/`attachments` field, collapsed.
    pub attachment_refs: Vec<AttachmentReferenceKind>,
}

impl ResultDraft {
    pub fn new(
        title: impl Into<String>,
        requirement_path: ReferencePath,
        requirement_commit: impl Into<String>,
        test_path: ReferencePath,
        test_commit: impl Into<String>,
    ) -> Self {
        ResultDraft {
            title: title.into(),
            requirement_path,
            requirement_commit: requirement_commit.into(),
            test_path,
            test_commit: test_commit.into(),
            status: StatusV1::default(),
            attachments: BTreeSet::new(),
            attachment_refs: Vec::new(),
        }
    }

    pub fn add_attachment(&mut self, path: &Path) -> Result<(), AddPoolFileError> {
        add_pool_file(&mut self.attachments, path)
    }

    pub fn remove_attachment(&mut self, path: &Path) -> bool {
        self.attachments.remove(path)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn minimal_result() -> ResultDraft {
        ResultDraft::new(
            "Title",
            ReferencePath("requirements/definition".to_string()),
            "abc",
            ReferencePath("tests/generic_test".to_string()),
            "def",
        )
    }

    #[test]
    fn new_defaults_to_incomplete() {
        let result = minimal_result();
        assert_eq!(result.title, "Title");
        assert!(matches!(result.status, StatusV1::Incomplete));
        assert!(result.attachments.is_empty());
    }

    #[test]
    fn add_and_remove_attachment_round_trip() {
        let mut result = minimal_result();
        result.add_attachment(Path::new("evidence.txt")).unwrap();
        assert!(result.attachments.contains(Path::new("evidence.txt")));
        assert!(result.remove_attachment(Path::new("evidence.txt")));
    }

    #[test]
    fn remove_attachment_is_false_when_absent() {
        let mut result = minimal_result();
        assert!(!result.remove_attachment(Path::new("evidence.txt")));
    }
}
