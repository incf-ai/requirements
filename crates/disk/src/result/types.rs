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
    /// Required; defaults to `Incomplete` if the field is absent from an
    /// on-disk `result.ron` (e.g. a hand-authored file that hasn't recorded
    /// an outcome yet).
    #[serde(default)]
    pub status: StatusV1,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub enum StatusV1 {
    Pass,
    Fail,
    #[default]
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
