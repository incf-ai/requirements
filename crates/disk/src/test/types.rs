use serde::{Deserialize, Serialize};

use crate::attachments::AttachmentFile;
use crate::util::EntryName;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TestDefinition {
    TestV1(TestV1),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TestV1 {
    pub title: String,
    pub result_kind: ResultKindV1,
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
}
