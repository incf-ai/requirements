use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::attachments::{AttachmentFile, AttachmentReferenceKind};
use crate::util::{EntryName, default_true};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TestDefinition {
    TestV1(TestV1),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde_with::skip_serializing_none]
pub struct TestV1 {
    pub title: String,
    pub result_kind: ResultKindV1,
    pub attachment: Option<AttachmentReferenceKind>,
    pub attachments: Option<nunny::Vec<AttachmentReferenceKind>>,
    pub template: Option<TemplateReferenceKind>,
    pub templates: Option<nunny::Vec<TemplateReferenceKind>>,
    /// Whether `attachments/` counts toward this test's `commit` (see
    /// `TestOnDisk::commit`). Defaults to `true` (included) when absent.
    #[serde(default = "default_true")]
    pub include_attachments_in_commit: bool,
    /// Whether `template/` counts toward this test's `commit` (see
    /// `TestOnDisk::commit`). Defaults to `true` (included) when absent.
    #[serde(default = "default_true")]
    pub include_template_in_commit: bool,
}

/// A reference to a template file, either physically local to this test's
/// own `template/` folder or shared at the module level in `templates/`
/// (as opposed to `AttachmentFile`, which is a file `disk` has actually
/// walked). Resolving this against the actual template list is out of
/// scope for `disk` — it only carries `name`/`path` as parsed from RON.
/// Shape mirrors `AttachmentReferenceKind`: `path` is where the file
/// actually is, `name` is a logical/display label free to differ from it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TemplateReferenceKind {
    /// A file in this test's own local `template/` folder.
    LocalTemplateReferenceV1 { name: EntryName, path: PathBuf },
    /// A file in this test's module's shared `templates/` folder.
    ModuleTemplateReferenceV1 { name: EntryName, path: PathBuf },
}

#[derive(Debug, Error)]
pub(crate) enum ValidateTestError {
    #[error("sets both `{singular}` and `{plural}` — use only one")]
    AmbiguousField {
        singular: &'static str,
        plural: &'static str,
    },
}

impl TestV1 {
    pub(crate) fn validate(&self) -> Result<(), ValidateTestError> {
        if self.attachment.is_some() && self.attachments.is_some() {
            return Err(ValidateTestError::AmbiguousField {
                singular: "attachment",
                plural: "attachments",
            });
        }
        if self.template.is_some() && self.templates.is_some() {
            return Err(ValidateTestError::AmbiguousField {
                singular: "template",
                plural: "templates",
            });
        }
        Ok(())
    }
}

/// How a result is expected to satisfy this test. Expected to grow more
/// variants over time (e.g. a programmatically-generated result kind).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ResultKindV1 {
    /// `template/` is a starting point/example; a satisfying result may
    /// attach whatever it wants, with no naming constraint against it.
    FreeForm,
    /// A satisfying result's attachments must share names with the files
    /// under `template/` (the result "fills in" the template file-for-file).
    Template,
}

/// A fully loaded `tests/<name>/` folder: the parsed `test.ron` plus its
/// sibling typst instructions, attachments, and result template.
#[derive(Debug, Clone)]
pub struct TestOnDisk {
    /// This test's directory name (e.g. `generic_test`), not to be confused
    /// with `definition.title`, a separate human-readable display title.
    pub name: EntryName,
    pub definition: TestV1,
    pub test_text: String,
    pub attachments: Vec<AttachmentFile>,
    pub template: Vec<AttachmentFile>,
    /// The newest git commit touching any file in this test's folder or its
    /// subfolders, resolved via `syscalls::Git::commit_for_path_excluding`
    /// at load time — not persisted in `test.ron`. Excludes `attachments/`
    /// and/or `template/` when `definition.include_attachments_in_commit`/
    /// `definition.include_template_in_commit` is `false`. `None` if the
    /// folder has never been committed (e.g. just saved by the GUI and not
    /// yet committed).
    pub commit: Option<String>,
}
