use serde::{Deserialize, Serialize};

use crate::attachments::AttachmentFile;
use crate::requirement::ReferencePath;
use crate::util::EntryName;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ResultDefinition {
    ResultsV1(ResultsV1),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde_with::skip_serializing_none]
pub struct ResultsV1 {
    pub title: String,
    pub path: ReferencePath,
    pub commit: String,
    pub status: Option<StatusV1>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum StatusV1 {
    Pass,
    Fail,
    Incomplete,
}

/// A fully loaded `results/<stage>/` folder: the parsed `result.ron` plus its
/// attachments.
#[derive(Debug, Clone)]
pub struct ResultOnDisk {
    /// This result's directory name (e.g. `definition`), not to be confused
    /// with `definition.title`, a separate human-readable display title.
    pub name: EntryName,
    pub definition: ResultsV1,
    pub attachments: Vec<AttachmentFile>,
}
