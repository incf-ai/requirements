use disk::{EntryName, ReferencePath};
use thiserror::Error;

/// The location of a requirement/test/result, as the sequence of submodule
/// names from the project root down to (but not including) its own
/// containing module, plus its own name. Empty `modules` means "directly
/// under the project root." See `crates/logical/README.md`'s "Validation
/// questions — answered" #1 for why this exists instead of a bare
/// `EntryName`: a leaf name alone isn't unique across the whole project,
/// since two different submodules can each have their own `generic_test`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalPath {
    pub modules: Vec<EntryName>,
    pub name: EntryName,
}

impl LogicalPath {
    pub fn root(name: EntryName) -> Self {
        LogicalPath {
            modules: Vec::new(),
            name,
        }
    }
}

/// The location of a result: the `LogicalPath` of its owning requirement,
/// plus its own name within that requirement's `results` map. Deliberately
/// not a `LogicalPath` itself — a result's `modules` chain would be
/// ambiguous with `LogicalPath::name` doing double duty as "requirement or
/// result?" — and, unlike a requirement/test, a result is never named by a
/// `disk::ReferencePath` reference string, so it never needs
/// `parse_reference_path`/`format_reference_path`'s grammar.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResultPath {
    pub requirement: LogicalPath,
    pub name: EntryName,
}

impl std::fmt::Display for LogicalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for module in &self.modules {
            write!(f, "modules/{module}/")?;
        }
        write!(f, "{}", self.name)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ParseReferencePathError {
    #[error("reference path `{path}` is too short to name a {expected_kind}")]
    TooShort {
        path: String,
        expected_kind: &'static str,
    },
    #[error("reference path `{path}` names a `{found}`, expected a `{expected_kind}`")]
    WrongKind {
        path: String,
        found: String,
        expected_kind: &'static str,
    },
    #[error("reference path `{path}` has a malformed module segment")]
    MalformedModuleSegment { path: String },
}

/// Parses a `disk::ReferencePath` (the raw, on-disk, possibly leading-slash
/// or module-relative string) into a concrete `LogicalPath`, given the
/// module the reference itself lives in. Doesn't check the target actually
/// exists — that's a separate lookup once this returns a `LogicalPath`.
pub(crate) fn parse_reference_path(
    raw: &ReferencePath,
    current_module: &[EntryName],
    expected_kind: &'static str,
) -> Result<LogicalPath, ParseReferencePathError> {
    let is_absolute = raw.0.starts_with('/');
    let trimmed = raw.0.trim_start_matches('/');
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();

    let Some(split_at) = segments.len().checked_sub(2) else {
        return Err(ParseReferencePathError::TooShort {
            path: raw.0.clone(),
            expected_kind,
        });
    };

    let (module_segments, kind_and_name) = segments.split_at(split_at);
    let kind = kind_and_name[0];
    let name = kind_and_name[1];
    if kind != expected_kind {
        return Err(ParseReferencePathError::WrongKind {
            path: raw.0.clone(),
            found: kind.to_string(),
            expected_kind,
        });
    }

    let mut modules = if is_absolute {
        Vec::new()
    } else {
        current_module.to_vec()
    };

    let mut segments = module_segments.iter();
    while let Some(&segment) = segments.next() {
        if segment != "modules" {
            return Err(ParseReferencePathError::MalformedModuleSegment {
                path: raw.0.clone(),
            });
        }
        let Some(&submodule) = segments.next() else {
            return Err(ParseReferencePathError::MalformedModuleSegment {
                path: raw.0.clone(),
            });
        };
        modules.push(EntryName(submodule.to_string()));
    }

    Ok(LogicalPath {
        modules,
        name: EntryName(name.to_string()),
    })
}

/// Inverse of `parse_reference_path`: renders `target` as a raw
/// `ReferencePath` string as seen from `current_module`. Prefers a
/// module-relative string (matching how `crates/logical/src/reference_repair.rs`
/// wants repaired references to keep their original style) but falls back to
/// an absolute one whenever `target` isn't reachable by descending from
/// `current_module` — relative addressing here can only dip into
/// submodules, never climb out to a sibling or ancestor (mirrors
/// `parse_reference_path`, which never resolves `..`).
pub(crate) fn format_reference_path(
    target: &LogicalPath,
    current_module: &[EntryName],
    prefer_absolute: bool,
    expected_kind: &'static str,
) -> ReferencePath {
    let relative_modules = (!prefer_absolute && target.modules.starts_with(current_module))
        .then(|| &target.modules[current_module.len()..]);

    let mut out = String::new();
    let modules = match relative_modules {
        Some(modules) => modules,
        None => {
            out.push('/');
            &target.modules[..]
        }
    };
    for module in modules {
        out.push_str("modules/");
        out.push_str(module.as_str());
        out.push('/');
    }
    out.push_str(expected_kind);
    out.push('/');
    out.push_str(target.name.as_str());
    ReferencePath(out)
}

/// Public counterpart to `parse_reference_path`, for callers outside this
/// crate — the read-only requirement viewer's clickable reference links —
/// that need to resolve a raw on-disk reference string into a `LogicalPath`
/// without going through `ValidatedProject`. Swallows the parse error rather
/// than exposing `ParseReferencePathError` (itself crate-private): an
/// unresolvable reference just isn't rendered as a link, no need for the
/// caller to distinguish why.
pub fn resolve_reference_path(
    raw: &ReferencePath,
    current_module: &[EntryName],
    expected_kind: &'static str,
) -> Option<LogicalPath> {
    parse_reference_path(raw, current_module, expected_kind).ok()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn display_matches_the_disk_directory_layout() {
        let path = LogicalPath {
            modules: vec![EntryName("embeddings".to_string())],
            name: EntryName("generic_test".to_string()),
        };
        assert_eq!(path.to_string(), "modules/embeddings/generic_test");
    }

    #[test]
    fn display_at_the_project_root_has_no_modules_prefix() {
        let path = LogicalPath::root(EntryName("generic_test".to_string()));
        assert_eq!(path.to_string(), "generic_test");
    }

    #[test]
    fn parses_an_absolute_reference_at_the_project_root() {
        let path = parse_reference_path(
            &ReferencePath("/tests/generic_inspection".to_string()),
            &[EntryName("embeddings".to_string())],
            "tests",
        )
        .unwrap();
        assert_eq!(
            path,
            LogicalPath::root(EntryName("generic_inspection".to_string()))
        );
    }

    #[test]
    fn parses_a_relative_reference_within_the_current_module() {
        let current = vec![EntryName("embeddings".to_string())];
        let path = parse_reference_path(
            &ReferencePath("requirements/discovery".to_string()),
            &current,
            "requirements",
        )
        .unwrap();
        assert_eq!(
            path,
            LogicalPath {
                modules: current,
                name: EntryName("discovery".to_string()),
            }
        );
    }

    #[test]
    fn parses_an_absolute_reference_through_nested_submodules() {
        let path = parse_reference_path(
            &ReferencePath("/modules/embeddings/tests/generic_test".to_string()),
            &[],
            "tests",
        )
        .unwrap();
        assert_eq!(
            path,
            LogicalPath {
                modules: vec![EntryName("embeddings".to_string())],
                name: EntryName("generic_test".to_string()),
            }
        );
    }

    #[test]
    fn reports_a_path_too_short_to_name_anything() {
        let err =
            parse_reference_path(&ReferencePath("tests".to_string()), &[], "tests").unwrap_err();
        assert!(matches!(err, ParseReferencePathError::TooShort { .. }));
    }

    #[test]
    fn reports_an_empty_path_as_too_short_rather_than_panicking() {
        let err = parse_reference_path(&ReferencePath(String::new()), &[], "tests").unwrap_err();
        assert!(matches!(err, ParseReferencePathError::TooShort { .. }));
    }

    #[test]
    fn reports_the_wrong_kind() {
        let err = parse_reference_path(
            &ReferencePath("/requirements/definition".to_string()),
            &[],
            "tests",
        )
        .unwrap_err();
        assert!(matches!(err, ParseReferencePathError::WrongKind { .. }));
    }

    #[test]
    fn reports_a_malformed_module_segment() {
        let err = parse_reference_path(
            &ReferencePath("/embeddings/tests/generic_test".to_string()),
            &[],
            "tests",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ParseReferencePathError::MalformedModuleSegment { .. }
        ));
    }

    #[test]
    fn formats_a_relative_reference_within_the_current_module() {
        let current = vec![EntryName("embeddings".to_string())];
        let path = format_reference_path(
            &LogicalPath {
                modules: current.clone(),
                name: EntryName("discovery".to_string()),
            },
            &current,
            false,
            "requirements",
        );
        assert_eq!(path.0, "requirements/discovery");
    }

    #[test]
    fn formats_a_relative_reference_dipping_into_a_submodule() {
        let current = vec![];
        let path = format_reference_path(
            &LogicalPath {
                modules: vec![EntryName("embeddings".to_string())],
                name: EntryName("generic_test".to_string()),
            },
            &current,
            false,
            "tests",
        );
        assert_eq!(path.0, "modules/embeddings/tests/generic_test");
    }

    #[test]
    fn formats_an_absolute_reference_at_the_project_root() {
        let path = format_reference_path(
            &LogicalPath::root(EntryName("generic_inspection".to_string())),
            &[EntryName("embeddings".to_string())],
            true,
            "tests",
        );
        assert_eq!(path.0, "/tests/generic_inspection");
    }

    #[test]
    fn formats_as_absolute_when_the_target_is_not_reachable_relatively() {
        // `current_module` is `embeddings`, but the target lives directly
        // under the project root — not a descendant of `embeddings` — so a
        // relative path (which can only dip *into* submodules) can't reach
        // it, even though the caller didn't ask for an absolute path.
        let path = format_reference_path(
            &LogicalPath::root(EntryName("definition".to_string())),
            &[EntryName("embeddings".to_string())],
            false,
            "requirements",
        );
        assert_eq!(path.0, "/requirements/definition");
    }

    #[test]
    fn format_and_parse_round_trip_for_relative_references() {
        let current = vec![EntryName("embeddings".to_string())];
        let target = LogicalPath {
            modules: current.clone(),
            name: EntryName("discovery".to_string()),
        };
        let raw = format_reference_path(&target, &current, false, "requirements");
        assert_eq!(
            parse_reference_path(&raw, &current, "requirements").unwrap(),
            target
        );
    }

    #[test]
    fn format_and_parse_round_trip_for_absolute_references() {
        let target = LogicalPath {
            modules: vec![EntryName("embeddings".to_string())],
            name: EntryName("generic_test".to_string()),
        };
        let raw = format_reference_path(&target, &[], true, "tests");
        assert_eq!(parse_reference_path(&raw, &[], "tests").unwrap(), target);
    }

    #[test]
    fn reports_a_module_segment_missing_its_name() {
        // The trailing kind+name pair ("tests", "generic_test") parses
        // fine, leaving a dangling "modules" keyword with no submodule
        // name after it.
        let err = parse_reference_path(
            &ReferencePath("/modules/tests/generic_test".to_string()),
            &[],
            "tests",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ParseReferencePathError::MalformedModuleSegment { .. }
        ));
    }
}
