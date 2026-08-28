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
- **`attachments/` is always required**, on the project root, every
  submodule, every requirement stage, every test, and every result folder —
  even if empty. Files under it are read recursively; each becomes a path
  (relative to `attachments/`) + the git commit that last touched it
  (`AttachmentFile { path, commit }`) — `disk` never reads or stores the raw
  bytes itself, so loading requires a `syscalls::Git` alongside the
  `Filesystem`. Saving only checks that each referenced file already exists
  on disk; it never writes attachment content either — except a `.gitkeep`
  placeholder, always dropped into the directory on save, so it stays
  present in git even when it holds no real attachments (git doesn't track
  empty directories). `read_attachments` ignores this placeholder.
- **A requirement/test/result folder's own `attachments/` vs. its module's.**
  The `attachments/` directly under a `requirements/<stage>/`, `tests/<name>/`,
  or `results/<stage>/` folder holds files physically local to that one
  entity, walked automatically into its `attachments: Vec<AttachmentFile>`.
  A *module's* `attachments/` (directly under the project root or a
  `modules/<name>/` folder) is a separate, shared pool, not walked
  automatically into anything.
- **`AttachmentReferenceKind`** is how a `requirement.ron`/`test.ron`/
  `result.ron`'s `attachment`/`attachments` field (see each section below)
  references one specific file, in one of those two places. Both variants
  carry a `name` *and* a `path`, kept deliberately separate — same
  relationship as `RequirementOnDisk::name` (a directory name) to
  `RequirementDefinitionV1::title` (freeform text): `path` is where the file
  actually is, `name` is a logical/display label that's free to differ from
  it:
  - `LocalAttachmentReferenceV1 { name: EntryName, path: PathBuf }` — a file
    in this entity's own local `attachments/` folder, at `path` (relative to
    that `attachments/` directory, may be nested).
  - `ModuleAttachmentReferenceV1 { name: EntryName, path: PathBuf }` — a
    file in this entity's module's shared `attachments/` folder, at `path`
    (relative to that `attachments/` directory, may be nested).
  - `disk` does not check that either actually resolves to a file in the
    corresponding attachment list — that cross-check spans the whole loaded
    tree and is deferred to a higher layer.
- **`TemplateReferenceKind`** is the same idea as `AttachmentReferenceKind`,
  scoped to tests only: how a `test.ron`'s `template`/`templates` field
  references a file, either in this test's own local `template/` folder or
  the module's shared `templates/` folder:
  - `LocalTemplateReferenceV1 { name: EntryName, path: PathBuf }`
  - `ModuleTemplateReferenceV1 { name: EntryName, path: PathBuf }`
  - Same `name`/`path` split and the same "not cross-checked" caveat as
    `AttachmentReferenceKind`.
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
  root (see below): `attachments/`, `templates/`, `requirements/`, `tests/`,
  `results/`, `modules/`.

## `modules/<name>/` (a submodule)

- `submodule.ron` — required. `SubmoduleV1(name: String)` — `name` is a
  free-form display name, independent of the directory name.
- `attachments/`, `templates/`, `requirements/`, `tests/`, `results/`,
  `modules/` — **all six required directories**, even when empty.
  `attachments/`/`templates/` are this module's own shared pools (see
  shared conventions above) — unlike the other four, they aren't keyed by
  child name, they're just flat lists of files. Each child inside the other
  four is loaded and keyed by its own directory name. `modules/` nests
  recursively — a submodule can contain further submodules to any depth,
  each with its own `attachments/`/`templates/`.

## `requirements/<stage>/` (a requirement)

- `requirement.ron` — required.
  ```
  RequirementDefinitionV1(
      title: String,
      test: Option<TestReferenceKind>,
      tests: Option<NonEmptyVec<TestReferenceKind>>,
      dependency: Option<DependencyReferenceKind>,
      dependencies: Option<NonEmptyVec<DependencyReferenceKind>>,
      attachment: Option<AttachmentReferenceKind>,
      attachments: Option<NonEmptyVec<AttachmentReferenceKind>>,
      include_attachments_in_commit: bool,
  )
  ```
  - **Rule: `test` and `tests` are mutually exclusive.** Setting both is
    rejected. Setting neither, or exactly one, is valid.
  - **Rule: `dependency` and `dependencies` are mutually exclusive**, same
    shape as above.
  - **Rule: `attachment` and `attachments` are mutually exclusive**, same
    shape as above.
  - `test`/`tests` reference test folders: `TestReferenceKind::TestReferenceV1(LocalGitReference)`.
  - `attachment`/`attachments` reference a file via `AttachmentReferenceKind`
    (see shared conventions above) — either local to this requirement stage
    or shared at the module level.
  - `dependency`/`dependencies` reference other requirement stages, one of:
    - `DependencyReferenceKind::RequirementReferenceV1(LocalGitReference)` —
      a stage elsewhere in this same project.
    - `DependencyReferenceKind::RemoteReferenceV1(RemoteGitReference)`
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
- The loaded `RequirementOnDisk` also carries a `commit: String` field: the
  newest git commit touching any file in this stage's folder or its
  subfolders. It's derived at load time
  (`syscalls::Git::commit_for_path_excluding` on the folder itself), not
  read from or written to `requirement.ron`.
  - `include_attachments_in_commit` (defaults to `true` when absent)
    controls whether `attachments/` counts toward that commit — `false`
    excludes it, so touching only files under `attachments/` won't change
    `commit`.

## `tests/<name>/` (a test)

- `test.ron` — required.
  ```
  TestV1(
      title: String,
      result_kind: ResultKindV1,
      attachment: Option<AttachmentReferenceKind>,
      attachments: Option<NonEmptyVec<AttachmentReferenceKind>>,
      template: Option<TemplateReferenceKind>,
      templates: Option<NonEmptyVec<TemplateReferenceKind>>,
      include_attachments_in_commit: bool,
      include_template_in_commit: bool,
  )
  ```
  - **Rule: `attachment` and `attachments` are mutually exclusive**, same as
    on `requirement.ron`, naming a file local to this test or shared at the
    module level (see `AttachmentReferenceKind` in shared conventions above).
  - **Rule: `template` and `templates` are mutually exclusive**, same shape
    as above, naming a file in this test's own local `template/` folder or
    the module's shared `templates/` folder (see `TemplateReferenceKind` in
    shared conventions above) — independent of `attachment`/`attachments`.
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
- The loaded `TestOnDisk` also carries a `commit: String` field, same idea
  as `RequirementOnDisk::commit` — the newest commit touching anything in
  this test's folder, computed via `syscalls::Git::commit_for_path_excluding`.
  `include_attachments_in_commit`/`include_template_in_commit` (each
  defaults to `true` when absent) independently control whether
  `attachments/`/`template/` count toward it.

## `results/<stage>/` (a result)

- `result.ron` — required.
  ```
  ResultsV1(
      title: String,
      requirement_path: ReferencePath,
      requirement_commit: String,
      test_path: ReferencePath,
      test_commit: String,
      status: StatusV1,
      attachment: Option<AttachmentReferenceKind>,
      attachments: Option<NonEmptyVec<AttachmentReferenceKind>>,
  )
  ```
  - `requirement_path`/`requirement_commit` record *which requirement, at
    what commit,* this result was produced against; `test_path`/
    `test_commit` record *which test, at what commit,* it satisfies —
    tracked independently since a requirement and the test it references
    don't necessarily change together. Unlike `LocalGitReference`, neither
    pair is a reference expected to still match something; both are fixed
    snapshots.
  - **Rule: `attachment` and `attachments` are mutually exclusive**, same as
    on `requirement.ron`, naming a file local to this result or shared at
    the module level (see `AttachmentReferenceKind` in shared conventions
    above).
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
variants are a private implementation detail. All filesystem and git access
goes through the [`syscalls`](../syscalls) crate's `Filesystem`/`Git` traits
rather than `std::fs`/shelling out to `git` directly, so tests can inject
arbitrary IO/git failures (missing file, permission denied, untracked path,
...) instead of depending on real disk or repository state.
