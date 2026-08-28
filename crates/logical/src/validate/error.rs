use std::path::PathBuf;

use disk::EntryName;
use thiserror::Error;

use crate::LogicalPath;

/// One rule violation found while validating a `ProjectDraft`. Unlike
/// `disk`'s opaque per-function `Error` wrappers, `ValidationError` is
/// fully public and matchable — see `crates/logical/README.md`'s
/// validation sections: a caller needs to introspect and group these for
/// real UX (print them, act on specific kinds), not just log-and-stop like
/// a single terminal disk IO error. `validate()` returns every violation
/// it finds, not just the first.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// A reference (test, dependency, attachment, or template) names
    /// something that doesn't exist. Grouped by the missing target — see
    /// the README's "Cascading validation errors": one error per missing
    /// thing, `referenced_by` lists everything that names it.
    #[error("{target} does not exist, but is referenced by {referenced_by:?}")]
    UnresolvedReference {
        target: UnresolvedTarget,
        referenced_by: Vec<LogicalPath>,
    },
    /// A set of local dependency edges form a cycle. `cycle` lists the
    /// requirements involved, in edge order (first == last).
    #[error("dependency cycle: {cycle:?}")]
    DependencyCycle { cycle: Vec<LogicalPath> },
    /// A local `attachments/`/`template/` pool has a file that isn't named
    /// by any `Local*ReferenceV1` entry on the entity that owns it — see
    /// "Validation questions — answered" #4.1. (The other direction — a
    /// declared local reference naming a file that isn't physically
    /// there — surfaces as `UnresolvedReference` instead, since that's
    /// already the general "reference names something missing" shape.)
    #[error("{entity} has undeclared files in its local {pool:?} pool: {undeclared:?}")]
    LocalPoolMismatch {
        entity: LogicalPath,
        pool: PoolKind,
        undeclared: Vec<PathBuf>,
    },
    /// A `Template`-kind test's result doesn't have an attachment for
    /// every one of the test's template files (matched by file name).
    #[error("{result} doesn't cover every template file of {test}: missing {missing_file_names:?}")]
    TemplateCoverageMismatch {
        test: LogicalPath,
        result: LogicalPath,
        missing_file_names: Vec<String>,
    },
    /// A `RemoteReferenceV1` dependency's commit couldn't be resolved —
    /// see "Validation questions — answered" #2: this is a normal
    /// `ValidationError`, not a distinct non-fatal category, even though
    /// the cause (network/remote availability) is external to the
    /// project's own content.
    #[error("{referenced_by} references remote `{url}`, which failed to resolve: {message}")]
    RemoteResolutionFailed {
        referenced_by: LogicalPath,
        url: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Error)]
pub enum UnresolvedTarget {
    #[error("requirement {0}")]
    Requirement(LogicalPath),
    #[error("test {0}")]
    Test(LogicalPath),
    #[error("local attachment `{path}` of {entity}", path = path.display())]
    LocalAttachment { entity: LogicalPath, path: PathBuf },
    #[error(
        "module attachment `{path}` (module {})",
        module.last().map(EntryName::as_str).unwrap_or("<project root>")
    )]
    ModuleAttachment {
        module: Vec<EntryName>,
        path: PathBuf,
    },
    #[error("local template `{path}` of {entity}", path = path.display())]
    LocalTemplate { entity: LogicalPath, path: PathBuf },
    #[error(
        "module template `{path}` (module {})",
        module.last().map(EntryName::as_str).unwrap_or("<project root>")
    )]
    ModuleTemplate {
        module: Vec<EntryName>,
        path: PathBuf,
    },
    #[error("malformed reference `{raw}`")]
    MalformedReference { raw: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    Attachments,
    Template,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn unresolved_reference_error_message_is_readable() {
        let err = ValidationError::UnresolvedReference {
            target: UnresolvedTarget::Requirement(LogicalPath::root(EntryName(
                "definition".to_string(),
            ))),
            referenced_by: vec![LogicalPath::root(EntryName("discovery".to_string()))],
        };
        assert!(err.to_string().contains("definition"));
    }

    #[test]
    fn dependency_cycle_error_message_is_readable() {
        let err = ValidationError::DependencyCycle {
            cycle: vec![LogicalPath::root(EntryName("a".to_string()))],
        };
        assert!(err.to_string().contains("dependency cycle"));
    }

    #[test]
    fn local_pool_mismatch_error_message_is_readable() {
        let err = ValidationError::LocalPoolMismatch {
            entity: LogicalPath::root(EntryName("definition".to_string())),
            pool: PoolKind::Attachments,
            undeclared: vec![PathBuf::from("extra.txt")],
        };
        assert!(err.to_string().contains("extra.txt"));
    }

    #[test]
    fn template_coverage_mismatch_error_message_is_readable() {
        let err = ValidationError::TemplateCoverageMismatch {
            test: LogicalPath::root(EntryName("generic_test".to_string())),
            result: LogicalPath::root(EntryName("definition".to_string())),
            missing_file_names: vec!["spec.typ".to_string()],
        };
        assert!(err.to_string().contains("spec.typ"));
    }

    #[test]
    fn remote_resolution_failed_error_message_is_readable() {
        let err = ValidationError::RemoteResolutionFailed {
            referenced_by: LogicalPath::root(EntryName("definition".to_string())),
            url: "https://example.com/repo.git".to_string(),
            message: "connection refused".to_string(),
        };
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn unresolved_target_module_attachment_names_the_root_when_module_path_is_empty() {
        let target = UnresolvedTarget::ModuleAttachment {
            module: vec![],
            path: PathBuf::from("logo.png"),
        };
        assert!(target.to_string().contains("<project root>"));
    }

    #[test]
    fn unresolved_target_module_template_names_the_deepest_submodule() {
        let target = UnresolvedTarget::ModuleTemplate {
            module: vec![EntryName("embeddings".to_string())],
            path: PathBuf::from("summary.txt"),
        };
        assert!(target.to_string().contains("embeddings"));
    }

    #[test]
    fn unresolved_target_malformed_reference_message_is_readable() {
        let target = UnresolvedTarget::MalformedReference {
            raw: "tests".to_string(),
        };
        assert!(target.to_string().contains("tests"));
    }

    #[test]
    fn unresolved_target_local_attachment_and_template_messages_are_readable() {
        let entity = LogicalPath::root(EntryName("definition".to_string()));
        let attachment = UnresolvedTarget::LocalAttachment {
            entity: entity.clone(),
            path: PathBuf::from("notes.txt"),
        };
        assert!(attachment.to_string().contains("notes.txt"));

        let template = UnresolvedTarget::LocalTemplate {
            entity,
            path: PathBuf::from("result.typ"),
        };
        assert!(template.to_string().contains("result.typ"));
    }
}
