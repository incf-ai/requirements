# disk

Loads and saves the project's on-disk format: RON (`.ron`) definition files
plus sibling typst (`.typ`) text and `attachments/`/`template/` directories,
as illustrated in full by [`sample_project/`](../../sample_project). This
crate only enforces *structural* rules — required files/directories exist,
`.ron` files parse, and a couple of cross-field rules called out below.
Anything that needs the full loaded tree (resolving a reference, checking a
result's attachments actually match a test's template) is out of scope here
and deferred to a higher layer.

Every rule below is enforced the same way on both `load_*` and `save_*` —
saving an in-memory value that violates a rule fails before anything is
written.

## Shared conventions

- **Directory name vs. display name/title.** Every loaded value has an
  `EntryName` (its own directory's name, e.g. `definition`) separate from
  any human-readable `title`/`name` field inside its `.ron` file. The two
  are allowed to differ — e.g. in the sample project, directory `setup`
  holds `submodule.ron`'s `name: "Setup"`.
- **`attachments/` is always required**, on every requirement stage, test,
  and result folder — even if empty. Files under it are read recursively;
  each becomes a path (relative to `attachments/`) + raw bytes.
- **On-disk values are versioned.** Every `.ron` file's top-level value is a
  wrapper enum with a single `V1` variant today (`RequirementDefinitionV1`,
  `TestV1`, `ResultsV1`, `SubmoduleV1`, `RootV1`) so a future `V2` can be
  added without breaking existing files.
- **Newtypes stay transparent on disk.** Wrapper types like `ReferencePath`
  and `EntryName` are written as plain strings in RON (via the `UNWRAP_NEWTYPES`
  extension) — they exist for type safety in Rust, not to change the format.

## `project/` (the project root)

- `project.ron` — required. `RootV1(name: String)`.
- Then the same required subtree as a submodule, directly under the project
  root (see below): `requirements/`, `tests/`, `results/`, `modules/`.

## `modules/<name>/` (a submodule)

- `submodule.ron` — required. `SubmoduleV1(name: String)` — `name` is a
  free-form display name, independent of the directory name.
- `requirements/`, `tests/`, `results/`, `modules/` — **all four required
  directories**, even when empty. Each child inside is loaded and keyed by
  its own directory name. `modules/` nests recursively — a submodule can
  contain further submodules to any depth.

## `requirements/<stage>/` (a requirement)

- `requirement.ron` — required.
  ```
  RequirementDefinitionV1(
      title: String,
      test: Option<TestReferenceKind>,
      tests: Option<NonEmptyVec<TestReferenceKind>>,
      dependency: Option<DependencyReferenceKind>,
      dependencies: Option<NonEmptyVec<DependencyReferenceKind>>,
  )
  ```
  - **Rule: `test` and `tests` are mutually exclusive.** Setting both is
    rejected. Setting neither, or exactly one, is valid.
  - **Rule: `dependency` and `dependencies` are mutually exclusive**, same
    shape as above.
  - `test`/`tests` reference test folders: `TestReferenceKind::TestReferenceV1(LocalGitReference)`.
  - `dependency`/`dependencies` reference other requirement stages, one of:
    - `DependencyReferenceKind::RequirementReferenceV1(LocalGitReference)` —
      a stage elsewhere in this same project.
    - `DependencyReferenceKind::RemoteRequirementReferenceV1(RemoteGitReference)`
      — a stage in a *different* git repository.
    - the bare `Submodules` variant, meaning "depends on all submodules
      being complete" rather than one specific requirement.
  - `LocalGitReference { path: ReferencePath, commit: String }` —
    **path convention**: a leading `/` means the path is relative to the
    *project root*; no leading slash means relative to the *current
    module's own root*.
  - `RemoteGitReference { url: String, path: Option<ReferencePath>, commit: String }`
    — same shape as `LocalGitReference` plus a `url` (`http(s)://`,
    `file://`, `ssh://`, ...) identifying the remote repository. `path` is
    **optional** — absent means the dependency is the remote repository
    itself (its project root), not some path within it. When present, its
    leading-slash convention doesn't apply here — it's always just a path
    within that remote repo, since there's no "current module"/"project
    root" relationship across repositories.
  - `commit` (on both reference kinds) is meant to always be a real,
    full-length git commit hash (disk stores it as an opaque string;
    resolving/verifying it against actual git history is not this crate's
    job).
- `requirement.typ` — required (content may be empty).
- `requirement_guidance.typ`, `test_guidance.typ` — **optional**; the file
  may be entirely absent (`None`), as opposed to `requirement.typ` which
  must exist even with empty content.
- `attachments/` — required (see shared conventions above).

## `tests/<name>/` (a test)

- `test.ron` — required. `TestV1(title: String, result_kind: ResultKindV1)`.
  - `ResultKindV1` is a plain, extensible enum (more variants expected over
    time, e.g. a programmatically-generated result kind):
    - `FreeForm` — `template/`'s files are a starting point/example only; a
      satisfying result may attach whatever it wants, with no naming
      constraint against the template.
    - `Template` — a satisfying result's attachments must share file names
      with `template/`'s files (the result "fills in" the template
      file-for-file). *(Checking that a given result actually satisfies
      this is a `logical`-layer concern, not enforced by `disk`.)*
- `test.typ` — required.
- `attachments/` — required.
- `template/` — required (may be empty), holding the files described above.

## `results/<stage>/` (a result)

- `result.ron` — required.
  ```
  ResultsV1(
      title: String,
      path: ReferencePath,
      commit: String,
      status: StatusV1,
  )
  ```
  - `path`/`commit` record *which requirement, at what commit,* this result
    was produced against — unlike `LocalGitReference`, this isn't a
    reference expected to still match something; it's a fixed snapshot.
  - `status: Pass | Fail | Incomplete` — **required**, but defaults to
    `Incomplete` if the field is absent from the file (e.g. a hand-authored
    `result.ron` that hasn't recorded an outcome yet); once loaded, every
    `ResultsV1` always has a concrete status, never "unset."
- `attachments/` — required.

## Error handling

Every public `load_*`/`save_*` function returns its own opaque `Error` type
(e.g. `disk::requirement::operations::load::Error`) rather than a shared
error enum, so each failure names exactly which file/directory/rule was the
problem; match on it only via `.to_string()`/`source()` — the concrete
variants are a private implementation detail. All filesystem (and, in the
future, git) access goes through the [`syscalls`](../syscalls) crate's
`Filesystem` trait rather than `std::fs` directly, so tests can inject
arbitrary IO failures (missing file, permission denied, ...) instead of
depending on real disk state.
