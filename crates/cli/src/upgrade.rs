use std::path::{Path, PathBuf};

use disk::{AttachmentReferenceKind, EntryName, ReferencePath, StatusV1};
use serde::{Deserialize, Serialize};
use syscalls::Filesystem;
use thiserror::Error;

/// One-shot bridge from the old flat `results/<name>/` layout (a sibling of
/// `requirements/`/`tests/` at each module level) to the new nested layout
/// (`requirements/<name>/results/<name>/`). This module is intentionally
/// self-contained and doesn't reuse `disk`'s own (crate-private) RON
/// helpers or `ResultsV1` type: `disk` no longer has any reason to know
/// about the old shape once every project has been upgraded, so this is a
/// temporary tool, not a permanent second reader built into `disk`.
///
/// Not idempotent across a partial failure: if a fault strikes partway
/// through migrating one module's `results/`, already-copied destination
/// directories from that run are left in place (nothing is ever deleted
/// until every result under a given flat `results/` dir has copied
/// successfully), so a bare rerun will report `DestinationAlreadyExists`
/// for those until they're cleaned up by hand. Acceptable for a one-shot
/// repair tool driven by a human, not worth the extra merge logic it would
/// take to make automatic.
mod ron_shapes {
    use super::*;

    pub(super) fn options() -> ron::Options {
        ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::EXPLICIT_STRUCT_NAMES)
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .with_default_extension(ron::extensions::Extensions::UNWRAP_NEWTYPES)
            .with_default_extension(ron::extensions::Extensions::UNWRAP_VARIANT_NEWTYPES)
    }

    /// Mirrors `disk::ResultDefinition`/`ResultsV1` as they exist in the old
    /// flat layout, `requirement_path` included. Read-only: this crate never
    /// writes this shape.
    #[derive(Debug, Clone, Deserialize)]
    pub(super) enum LegacyResultDefinition {
        ResultsV1(LegacyResultsV1),
    }

    #[derive(Debug, Clone, Deserialize)]
    pub(super) struct LegacyResultsV1 {
        pub title: String,
        pub requirement_path: ReferencePath,
        pub requirement_commit: String,
        pub test_path: ReferencePath,
        pub test_commit: String,
        #[serde(default)]
        pub status: StatusV1,
        pub attachment: Option<AttachmentReferenceKind>,
        pub attachments: Option<nunny::Vec<AttachmentReferenceKind>>,
    }

    /// The new, nested-layout shape: identical to `LegacyResultsV1` minus
    /// `requirement_path`, which becomes redundant once a result's parent
    /// directory is its requirement. Write-only: this crate never reads
    /// this shape back (nothing needs to, post-migration `disk` does).
    #[derive(Debug, Serialize)]
    pub(super) enum NewResultDefinition {
        ResultsV1(NewResultsV1),
    }

    #[derive(Debug, Serialize)]
    #[serde_with::skip_serializing_none]
    pub(super) struct NewResultsV1 {
        pub title: String,
        pub requirement_commit: String,
        pub test_path: ReferencePath,
        pub test_commit: String,
        pub status: StatusV1,
        pub attachment: Option<AttachmentReferenceKind>,
        pub attachments: Option<nunny::Vec<AttachmentReferenceKind>>,
    }
}

use ron_shapes::{LegacyResultDefinition, NewResultDefinition, NewResultsV1};

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("io error reading directory {path}: {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse RON at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: ron::de::SpannedError,
    },
    #[error("failed to serialize RON for {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: ron::Error,
    },
    #[error("failed to write {path}: {source}")]
    WriteFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("result at {result} has a malformed requirement_path `{raw}`")]
    MalformedRequirementPath { result: PathBuf, raw: String },
    #[error(
        "result at {result} names requirement directory {requirement_dir}, which doesn't exist"
    )]
    RequirementNotFound {
        result: PathBuf,
        requirement_dir: PathBuf,
    },
    #[error("migration destination {0} already exists")]
    DestinationAlreadyExists(PathBuf),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

/// How many results were moved, for reporting back to the user.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub migrated: usize,
}

/// Rewrites every flat `results/<name>/` tree under `project_root` (at the
/// project root itself, and recursively under every `modules/<name>/`) into
/// the nested `requirements/<name>/results/<name>/` layout, and ensures
/// every requirement ends up with a `results/` directory of its own (empty
/// if it never had any results) — `disk` treats a requirement's `results/`
/// as required, mirroring `attachments/`, so a requirement that happens to
/// have no results still needs the directory to exist. A project with no
/// flat `results/` directories anywhere still migrates cleanly (planting
/// empty `results/` dirs as needed); `Summary::migrated` counts only
/// results actually moved, so it's `0` in that case. Safe to run against an
/// already-fully-nested project (a true no-op).
pub fn upgrade_results_layout(fs: &dyn Filesystem, project_root: &Path) -> Result<Summary, Error> {
    let mut summary = Summary::default();
    upgrade_module(fs, project_root, project_root, &[], &mut summary).map_err(Error)?;
    Ok(summary)
}

fn upgrade_module(
    fs: &dyn Filesystem,
    project_root: &Path,
    module_dir: &Path,
    current_module: &[EntryName],
    summary: &mut Summary,
) -> Result<(), ErrorKind> {
    migrate_results_dir(fs, project_root, module_dir, current_module, summary)?;
    ensure_requirement_results_dirs(fs, module_dir)?;

    let modules_dir = module_dir.join("modules");
    if fs.is_dir(&modules_dir) {
        for entry in read_dir_sorted(fs, &modules_dir)? {
            if !fs.is_dir(&entry) {
                continue;
            }
            let mut nested_module = current_module.to_vec();
            nested_module.push(entry_name(&entry));
            upgrade_module(fs, project_root, &entry, &nested_module, summary)?;
        }
    }

    Ok(())
}

/// Plants an empty `results/` directory under every requirement in
/// `module_dir` that doesn't already have one — either because it never
/// owned any results, or because `migrate_results_dir` already ran above
/// and only created `results/` under requirements that actually received
/// one.
fn ensure_requirement_results_dirs(
    fs: &dyn Filesystem,
    module_dir: &Path,
) -> Result<(), ErrorKind> {
    let requirements_dir = module_dir.join("requirements");
    if !fs.is_dir(&requirements_dir) {
        return Ok(());
    }

    for entry in read_dir_sorted(fs, &requirements_dir)? {
        if !fs.is_dir(&entry) {
            continue;
        }
        let results_dir = entry.join("results");
        if !fs.exists(&results_dir) {
            fs.create_dir_all(&results_dir)
                .map_err(|source| ErrorKind::CreateDir {
                    path: results_dir,
                    source,
                })?;
        }
    }

    Ok(())
}

fn migrate_results_dir(
    fs: &dyn Filesystem,
    project_root: &Path,
    module_dir: &Path,
    current_module: &[EntryName],
    summary: &mut Summary,
) -> Result<(), ErrorKind> {
    let results_dir = module_dir.join("results");
    if !fs.is_dir(&results_dir) {
        return Ok(());
    }

    for entry in read_dir_sorted(fs, &results_dir)? {
        if !fs.is_dir(&entry) {
            continue;
        }
        migrate_one_result(fs, project_root, current_module, &entry)?;
        summary.migrated += 1;
    }

    fs.remove_dir_all(&results_dir)
        .map_err(|source| ErrorKind::Remove {
            path: results_dir,
            source,
        })
}

fn migrate_one_result(
    fs: &dyn Filesystem,
    project_root: &Path,
    current_module: &[EntryName],
    result_dir: &Path,
) -> Result<(), ErrorKind> {
    let ron_path = result_dir.join("result.ron");
    let contents = fs
        .read_to_string(&ron_path)
        .map_err(|source| ErrorKind::ReadFile {
            path: ron_path.clone(),
            source,
        })?;
    let LegacyResultDefinition::ResultsV1(legacy) = ron_shapes::options()
        .from_str(&contents)
        .map_err(|source| ErrorKind::Parse {
            path: ron_path.clone(),
            source,
        })?;

    let requirement_dir = resolve_requirement_dir(
        project_root,
        current_module,
        result_dir,
        &legacy.requirement_path,
    )?;
    if !fs.is_dir(&requirement_dir) {
        return Err(ErrorKind::RequirementNotFound {
            result: result_dir.to_path_buf(),
            requirement_dir,
        });
    }

    let dest_dir = requirement_dir
        .join("results")
        .join(entry_name(result_dir).as_str());
    if fs.exists(&dest_dir) {
        return Err(ErrorKind::DestinationAlreadyExists(dest_dir));
    }

    copy_dir_recursive(
        fs,
        &result_dir.join("attachments"),
        &dest_dir.join("attachments"),
    )?;

    let new_definition = NewResultDefinition::ResultsV1(NewResultsV1 {
        title: legacy.title,
        requirement_commit: legacy.requirement_commit,
        test_path: legacy.test_path,
        test_commit: legacy.test_commit,
        status: legacy.status,
        attachment: legacy.attachment,
        attachments: legacy.attachments,
    });
    let dest_ron_path = dest_dir.join("result.ron");
    let serialized = ron_shapes::options()
        .to_string_pretty(&new_definition, ron::ser::PrettyConfig::default())
        .map_err(|source| ErrorKind::Serialize {
            path: dest_ron_path.clone(),
            source,
        })?;
    fs.write(&dest_ron_path, serialized.as_bytes())
        .map_err(|source| ErrorKind::WriteFile {
            path: dest_ron_path,
            source,
        })
}

/// Parses a `disk::ReferencePath` the same way
/// `logical::path::parse_reference_path` does for the `"requirements"`
/// kind, but resolves straight to a physical directory under
/// `project_root` instead of a `LogicalPath` — this tool runs before
/// `logical` (or even `disk`'s new nested reader) can see the project at
/// all, so it can't go through either.
fn resolve_requirement_dir(
    project_root: &Path,
    current_module: &[EntryName],
    result_dir: &Path,
    raw: &ReferencePath,
) -> Result<PathBuf, ErrorKind> {
    let malformed = || ErrorKind::MalformedRequirementPath {
        result: result_dir.to_path_buf(),
        raw: raw.0.clone(),
    };

    let is_absolute = raw.0.starts_with('/');
    let trimmed = raw.0.trim_start_matches('/');
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err(malformed());
    }

    let (module_segments, kind_and_name) = segments.split_at(segments.len() - 2);
    if kind_and_name[0] != "requirements" {
        return Err(malformed());
    }
    let name = kind_and_name[1];

    let mut modules: Vec<EntryName> = if is_absolute {
        Vec::new()
    } else {
        current_module.to_vec()
    };
    let mut iter = module_segments.iter();
    while let Some(&segment) = iter.next() {
        if segment != "modules" {
            return Err(malformed());
        }
        let Some(&submodule) = iter.next() else {
            return Err(malformed());
        };
        modules.push(EntryName(submodule.to_string()));
    }

    let mut dir = project_root.to_path_buf();
    for module in &modules {
        dir = dir.join("modules").join(module.as_str());
    }
    Ok(dir.join("requirements").join(name))
}

fn entry_name(dir: &Path) -> EntryName {
    EntryName(
        dir.file_name()
            .expect("directory has a file name")
            .to_string_lossy()
            .into_owned(),
    )
}

fn read_dir_sorted(fs: &dyn Filesystem, dir: &Path) -> Result<Vec<PathBuf>, ErrorKind> {
    let mut entries = fs.read_dir(dir).map_err(|source| ErrorKind::ReadDir {
        path: dir.to_path_buf(),
        source,
    })?;
    entries.sort();
    Ok(entries)
}

fn copy_dir_recursive(fs: &dyn Filesystem, src: &Path, dst: &Path) -> Result<(), ErrorKind> {
    fs.create_dir_all(dst)
        .map_err(|source| ErrorKind::CreateDir {
            path: dst.to_path_buf(),
            source,
        })?;

    for entry in read_dir_sorted(fs, src)? {
        let dest_entry = dst.join(entry.file_name().expect("directory entry has a file name"));
        if fs.is_dir(&entry) {
            copy_dir_recursive(fs, &entry, &dest_entry)?;
        } else {
            let bytes = fs.read(&entry).map_err(|source| ErrorKind::ReadFile {
                path: entry.clone(),
                source,
            })?;
            fs.write(&dest_entry, &bytes)
                .map_err(|source| ErrorKind::WriteFile {
                    path: dest_entry,
                    source,
                })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use syscalls::{FaultInjectingFilesystem, StdFilesystem};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cli-upgrade-{name}-{}-{}",
            std::process::id(),
            line!()
        ))
    }

    /// Builds a minimal old-layout project: a project root with one
    /// requirement and one module-level result naming it, using `--path`
    /// style absolute `requirement_path`s throughout (module-relative and
    /// nested-submodule variants get their own dedicated tests below).
    fn legacy_project(dir: &Path) {
        std::fs::create_dir_all(dir.join("requirements/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("requirements/definition/requirement.ron"),
            r#"RequirementDefinitionV1(title: "Definition")"#,
        )
        .unwrap();
        std::fs::write(dir.join("requirements/definition/requirement.typ"), "").unwrap();

        std::fs::create_dir_all(dir.join("results/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("results/definition/result.ron"),
            r#"ResultsV1(
                title: "Definition",
                requirement_path: "/requirements/definition",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();
    }

    #[test]
    fn migrates_a_module_root_result_under_its_requirement() {
        let dir = temp_dir("basic");
        legacy_project(&dir);

        let summary = upgrade_results_layout(&StdFilesystem, &dir).unwrap();
        assert_eq!(summary.migrated, 1);

        assert!(!dir.join("results").exists());
        let dest = dir.join("requirements/definition/results/definition");
        assert!(dest.join("result.ron").exists());
        let written = std::fs::read_to_string(dest.join("result.ron")).unwrap();
        assert!(!written.contains("requirement_path"));
        assert!(written.contains("Definition"));
        assert!(written.contains("Pass"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrates_a_result_with_attachments() {
        let dir = temp_dir("attachments");
        legacy_project(&dir);
        std::fs::write(
            dir.join("results/definition/attachments/evidence.txt"),
            "hello",
        )
        .unwrap();

        upgrade_results_layout(&StdFilesystem, &dir).unwrap();

        let dest_attachment =
            dir.join("requirements/definition/results/definition/attachments/evidence.txt");
        assert_eq!(std::fs::read_to_string(dest_attachment).unwrap(), "hello");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrates_a_module_relative_requirement_path() {
        let dir = temp_dir("module-relative");
        legacy_project(&dir);
        std::fs::write(
            dir.join("results/definition/result.ron"),
            r#"ResultsV1(
                title: "Definition",
                requirement_path: "requirements/definition",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();

        let summary = upgrade_results_layout(&StdFilesystem, &dir).unwrap();
        assert_eq!(summary.migrated, 1);
        assert!(
            dir.join("requirements/definition/results/definition/result.ron")
                .exists()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrates_results_belonging_to_a_submodule_requirement() {
        let dir = temp_dir("submodule");
        std::fs::create_dir_all(dir.join("modules/embeddings/requirements/definition/attachments"))
            .unwrap();
        std::fs::write(
            dir.join("modules/embeddings/requirements/definition/requirement.ron"),
            r#"RequirementDefinitionV1(title: "Definition")"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("modules/embeddings/requirements/definition/requirement.typ"),
            "",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("modules/embeddings/results/definition/attachments"))
            .unwrap();
        std::fs::write(
            dir.join("modules/embeddings/results/definition/result.ron"),
            r#"ResultsV1(
                title: "Definition",
                requirement_path: "requirements/definition",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();

        let summary = upgrade_results_layout(&StdFilesystem, &dir).unwrap();
        assert_eq!(summary.migrated, 1);
        assert!(!dir.join("modules/embeddings/results").exists());
        assert!(
            dir.join("modules/embeddings/requirements/definition/results/definition/result.ron")
                .exists()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absolute_reference_from_within_a_submodule_reaches_the_project_root() {
        let dir = temp_dir("submodule-absolute");
        legacy_project(&dir);
        std::fs::create_dir_all(dir.join("modules/embeddings/results/uses_root/attachments"))
            .unwrap();
        std::fs::write(
            dir.join("modules/embeddings/results/uses_root/result.ron"),
            r#"ResultsV1(
                title: "Uses Root",
                requirement_path: "/requirements/definition",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();

        upgrade_results_layout(&StdFilesystem, &dir).unwrap();

        assert!(
            dir.join("requirements/definition/results/uses_root/result.ron")
                .exists()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_project_with_no_flat_results_directories_is_a_no_op() {
        let dir = temp_dir("no-op");
        std::fs::create_dir_all(dir.join("requirements/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("requirements/definition/requirement.ron"),
            r#"RequirementDefinitionV1(title: "Definition")"#,
        )
        .unwrap();
        std::fs::write(dir.join("requirements/definition/requirement.typ"), "").unwrap();

        let summary = upgrade_results_layout(&StdFilesystem, &dir).unwrap();
        assert_eq!(summary.migrated, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `disk` treats a requirement's `results/` directory as required, the
    /// same way `attachments/` already is, even for a requirement that
    /// never had any results — so the upgrade tool must plant an empty one
    /// rather than leaving such a requirement unmigratable.
    #[test]
    fn plants_an_empty_results_dir_under_a_requirement_that_never_had_any() {
        let dir = temp_dir("empty-results-dir");
        std::fs::create_dir_all(dir.join("requirements/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("requirements/definition/requirement.ron"),
            r#"RequirementDefinitionV1(title: "Definition")"#,
        )
        .unwrap();
        std::fs::write(dir.join("requirements/definition/requirement.typ"), "").unwrap();

        upgrade_results_layout(&StdFilesystem, &dir).unwrap();
        assert!(dir.join("requirements/definition/results").is_dir());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn running_twice_is_a_no_op_the_second_time() {
        let dir = temp_dir("rerun");
        legacy_project(&dir);

        assert_eq!(
            upgrade_results_layout(&StdFilesystem, &dir)
                .unwrap()
                .migrated,
            1
        );
        assert_eq!(
            upgrade_results_layout(&StdFilesystem, &dir)
                .unwrap()
                .migrated,
            0
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_malformed_requirement_path_is_reported() {
        let dir = temp_dir("malformed-path");
        std::fs::create_dir_all(dir.join("results/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("results/definition/result.ron"),
            r#"ResultsV1(
                title: "Definition",
                requirement_path: "not-a-path",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();

        let err = upgrade_results_layout(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::MalformedRequirementPath { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_requirement_path_naming_a_wrong_kind_is_reported() {
        let dir = temp_dir("wrong-kind");
        std::fs::create_dir_all(dir.join("results/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("results/definition/result.ron"),
            r#"ResultsV1(
                title: "Definition",
                requirement_path: "/tests/generic_test",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();

        let err = upgrade_results_layout(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::MalformedRequirementPath { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_requirement_path_naming_a_nonexistent_requirement_is_reported() {
        let dir = temp_dir("missing-requirement");
        std::fs::create_dir_all(dir.join("results/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("results/definition/result.ron"),
            r#"ResultsV1(
                title: "Definition",
                requirement_path: "/requirements/nonexistent",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();

        let err = upgrade_results_layout(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::RequirementNotFound { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_preexisting_destination_directory_is_reported_rather_than_overwritten() {
        let dir = temp_dir("dest-exists");
        legacy_project(&dir);
        std::fs::create_dir_all(dir.join("requirements/definition/results/definition")).unwrap();

        let err = upgrade_results_layout(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::DestinationAlreadyExists(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_malformed_result_ron_reports_a_parse_error() {
        let dir = temp_dir("malformed-ron");
        std::fs::create_dir_all(dir.join("results/definition/attachments")).unwrap();
        std::fs::write(
            dir.join("results/definition/result.ron"),
            "not valid ron {{{",
        )
        .unwrap();

        let err = upgrade_results_layout(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::Parse { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_result_ron_reports_a_read_error() {
        let dir = temp_dir("missing-ron");
        std::fs::create_dir_all(dir.join("results/definition")).unwrap();

        let err = upgrade_results_layout(&StdFilesystem, &dir).unwrap_err();
        assert!(matches!(err.0, ErrorKind::ReadFile { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fault partway through copying a second result's attachments must
    /// not touch the first result (already copied) or delete the flat
    /// `results/` directory — see this module's own doc comment on
    /// non-idempotent partial-failure behavior.
    #[test]
    fn a_mid_migration_fault_leaves_the_flat_results_directory_intact() {
        let dir = temp_dir("mid-fault");
        legacy_project(&dir);
        std::fs::create_dir_all(dir.join("requirements/broken/attachments")).unwrap();
        std::fs::write(
            dir.join("requirements/broken/requirement.ron"),
            r#"RequirementDefinitionV1(title: "Broken")"#,
        )
        .unwrap();
        std::fs::write(dir.join("requirements/broken/requirement.typ"), "").unwrap();
        std::fs::create_dir_all(dir.join("results/zzz_broken/attachments")).unwrap();
        std::fs::write(
            dir.join("results/zzz_broken/attachments/evidence.txt"),
            "hello",
        )
        .unwrap();
        std::fs::write(
            dir.join("results/zzz_broken/result.ron"),
            r#"ResultsV1(
                title: "Broken",
                requirement_path: "/requirements/broken",
                requirement_commit: "deadbeef",
                test_path: "/tests/generic_test",
                test_commit: "deadbeef",
                status: Pass,
            )"#,
        )
        .unwrap();

        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("requirements/broken/results/zzz_broken/attachments/evidence.txt"),
            std::io::ErrorKind::PermissionDenied,
        );

        // "definition" sorts before "zzz_broken", so it's migrated
        // successfully before the fault hits.
        upgrade_results_layout(&fs, &dir).unwrap_err();

        assert!(
            dir.join("results/definition").exists(),
            "flat results/ dir must survive a later failure"
        );
        assert!(dir.join("results/zzz_broken").exists());
        assert!(
            dir.join("requirements/definition/results/definition/result.ron")
                .exists()
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
