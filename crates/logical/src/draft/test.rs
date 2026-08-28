use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use disk::{AttachmentReferenceKind, ResultKindV1, TemplateReferenceKind};

use crate::pool::{AddPoolFileError, add_pool_file};

/// A test, freely editable. `template`/`templates` collapses into
/// `template_refs`, same as `attachment`/`attachments` — see
/// `crates/logical/README.md`'s data model and "Decisions made" #1.
#[derive(Debug, Clone)]
pub struct TestDraft {
    pub title: String,
    pub result_kind: ResultKindV1,
    /// Files physically local to this test's own `attachments/`.
    pub attachments: BTreeSet<PathBuf>,
    /// This test's `attachment`/`attachments` field, collapsed.
    pub attachment_refs: Vec<AttachmentReferenceKind>,
    /// Files physically local to this test's own `template/`.
    pub template: BTreeSet<PathBuf>,
    /// This test's `template`/`templates` field, collapsed.
    pub template_refs: Vec<TemplateReferenceKind>,
    pub include_attachments_in_commit: bool,
    pub include_template_in_commit: bool,
    /// See `RequirementDraft::commit` — same idea, from
    /// `disk::TestOnDisk::commit`.
    pub commit: Option<String>,
}

impl TestDraft {
    pub fn new(title: impl Into<String>, result_kind: ResultKindV1) -> Self {
        TestDraft {
            title: title.into(),
            result_kind,
            attachments: BTreeSet::new(),
            attachment_refs: Vec::new(),
            template: BTreeSet::new(),
            template_refs: Vec::new(),
            include_attachments_in_commit: true,
            include_template_in_commit: true,
            commit: None,
        }
    }

    pub fn add_attachment(&mut self, path: &Path) -> Result<(), AddPoolFileError> {
        add_pool_file(&mut self.attachments, path)
    }

    pub fn remove_attachment(&mut self, path: &Path) -> bool {
        self.attachments.remove(path)
    }

    pub fn add_template_file(&mut self, path: &Path) -> Result<(), AddPoolFileError> {
        add_pool_file(&mut self.template, path)
    }

    pub fn remove_template_file(&mut self, path: &Path) -> bool {
        self.template.remove(path)
    }
}

#[cfg(test)]
#[allow(clippy::module_inception)]
mod test {
    use super::*;

    #[test]
    fn new_starts_empty_with_everything_included_in_commit() {
        let test = TestDraft::new("Title", ResultKindV1::FreeForm);
        assert_eq!(test.title, "Title");
        assert!(test.attachments.is_empty());
        assert!(test.template.is_empty());
        assert!(test.include_attachments_in_commit);
        assert!(test.include_template_in_commit);
        assert_eq!(test.commit, None);
    }

    #[test]
    fn add_and_remove_attachment_round_trip() {
        let mut test = TestDraft::new("Title", ResultKindV1::FreeForm);
        test.add_attachment(Path::new("checklist.md")).unwrap();
        assert!(test.attachments.contains(Path::new("checklist.md")));
        assert!(test.remove_attachment(Path::new("checklist.md")));
    }

    #[test]
    fn remove_attachment_is_false_when_absent() {
        let mut test = TestDraft::new("Title", ResultKindV1::FreeForm);
        assert!(!test.remove_attachment(Path::new("checklist.md")));
    }

    #[test]
    fn add_and_remove_template_file_round_trip() {
        let mut test = TestDraft::new("Title", ResultKindV1::FreeForm);
        test.add_template_file(Path::new("result.typ")).unwrap();
        assert!(test.template.contains(Path::new("result.typ")));
        assert!(test.remove_template_file(Path::new("result.typ")));
    }

    #[test]
    fn remove_template_file_is_false_when_absent() {
        let mut test = TestDraft::new("Title", ResultKindV1::FreeForm);
        assert!(!test.remove_template_file(Path::new("result.typ")));
    }
}
