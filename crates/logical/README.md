# logical

**Status: design phase — nothing in `src/` reflects this plan yet.** The
current `src/*.rs` files are an early sketch that predates `disk`'s current
shape (they embed raw `Vec<u8>` template content, which `disk` deliberately
no longer stores). This README is the plan for what replaces them.

## What this crate is for

`disk` loads/saves the on-disk format and enforces only *structural* rules:
required files exist, `.ron` parses, a handful of same-file cross-field
rules (`test`/`tests` mutually exclusive, etc.). It explicitly punts
everything that needs the *whole loaded tree* to "a higher layer" — that
layer is `logical`. Concretely, `logical` is responsible for:

- **Resolving references** `disk` only carries as unverified path/name
  data: `TestReferenceKind`, `DependencyReferenceKind`,
  `AttachmentReferenceKind`, `TemplateReferenceKind`. Resolving means
  checking the named target actually exists and getting a handle to it.
- **Cross-entity validation**: a `Template`-kind test's satisfying results
  actually filling in every template file; dependency references forming
  no cycle; a `test`/`tests` reference actually pointing at a test that
  exists; etc.
- **Editing**: letting a caller build up a set of changes — add a
  requirement, delete a test, rewire a dependency — across multiple
  entities, in any order, without `disk`'s file-by-file validation
  rejecting every intermediate state.
- **Queries that need the whole tree**: "is this requirement met",
  "what's the dependency chain for X", eventually surfaced to whatever
  calls this crate (CLI, TUI, LSP, ...).

`disk` stays the on-disk format layer; `logical` is the in-memory model of
a project once you want to *reason about* or *edit* it as a whole, not just
read/write one folder at a time.

## Data model

### BTreeMaps keyed by name, not Vecs

`disk::ModuleTree` holds `Vec<RequirementOnDisk>` etc. — order comes from
sorted directory listing, uniqueness isn't enforced by the type (two
entries could in principle carry the same `EntryName`, though in practice
they can't, since they physically come from listing a directory once).

For `logical`, switching the per-kind collections to
`BTreeMap<EntryName, T>` is worth doing, for one concrete reason:
reference resolution. Every reference `disk` hands us (`test`,
`dependency`, and now `attachment`/`template`, all by name/path) turns
into a lookup by key. A `BTreeMap` makes that lookup O(log n) and makes
"does this name already exist" (needed by every `add_*` operation) equally
cheap, instead of an O(n) scan over a `Vec`. It also gets us "iterate in a
stable, sorted order" for free, matching `disk`'s existing sort-by-name/
sort-by-path behavior — so converting back to `disk`'s `Vec`-shaped types
for saving is just `.values().cloned().collect()`.

```rust
pub struct LogicalModule {
    pub requirements: BTreeMap<EntryName, LogicalRequirement>,
    pub tests: BTreeMap<EntryName, LogicalTest>,
    pub results: BTreeMap<EntryName, LogicalResult>,
    pub modules: BTreeMap<EntryName, LogicalModule>,
    pub attachments: BTreeMap<PathBuf, LogicalAttachment>,
    pub templates: BTreeMap<PathBuf, LogicalAttachment>,
}

pub struct LogicalProject {
    pub definition: RootV1,
    pub tree: LogicalModule,
}
```

**`EntryName` for the four named children, `PathBuf` for the two
attachment pools — not uniformly one or the other.** Requirements, tests,
results, and submodules are each a single directory, so `EntryName` (their
directory name, already how `disk` identifies them) is the natural,
already-unique key. Attachments and templates are *files*, possibly
nested (`AttachmentFile.path` supports subdirectories), and a reference
resolves by `path`, not by the reference's own `name` field — `name` on
`AttachmentReferenceKind`/`TemplateReferenceKind` is a free-form label the
RON author chose for their own readability, not an identifier `disk`
enforces uniqueness on. So the pools have to be keyed by the thing that's
actually unique on disk: the path. (This does mean two attachments could
have the same `name` — that's fine, `name` was never meant to be a key.)

Every requirement/test/result also keeps its own **local** attachments the
same way — `LogicalRequirement.attachments: BTreeMap<PathBuf,
LogicalAttachment>`, `LogicalTest.template: BTreeMap<PathBuf,
LogicalAttachment>` — mirroring `disk`'s local-vs-module-level split
one-for-one.

### Per-entity types

Each `Logical*` type is roughly `disk`'s `*OnDisk` type, minus the parts
that were only there to survive an RON round-trip, plus resolved handles
where `disk` only had a name/path:

```rust
pub struct LogicalRequirement {
    pub title: String,
    pub requirement_text: String,
    pub requirement_guidance: Option<String>,
    pub test_guidance: Option<String>,
    pub tests: Vec<ResolvedTestReference>,
    pub dependencies: Vec<ResolvedDependencyReference>,
    pub attachments: BTreeMap<PathBuf, LogicalAttachment>,
    pub attachment_refs: Vec<ResolvedAttachmentReference>,
    pub include_attachments_in_commit: bool,
}
```

**Decided: collapse.** `test`/`tests` and `dependency`/`dependencies`
(and `attachment`/`attachments`, `template`/`templates`) each become a
single `Vec` in the logical model — the *mutual exclusion* was a
RON-authoring convenience `disk` enforces; once loaded, "one test" and a
one-element list of tests mean the same thing, so `logical` doesn't carry
the distinction forward. (Consequence: `logical` → RON round-tripping is
lossy on which form the author wrote — a single `Vec` with one element
always re-serializes as the singular field, not necessarily how the
original file had it. Accepted trade-off.)

**What does "resolved" mean concretely?** Not a `Rc`/pointer — this crate
should stay in plain owned data, no shared mutable state, matching `disk`'s
style. A `Resolved*Reference` is the original reference info (so it can be
re-serialized) plus confirmation the target exists, e.g.:

```rust
pub struct ResolvedTestReference {
    pub path: ReferencePath,
    pub commit: String,
    // Resolved: the full project-root-relative location this points at,
    // computed once during validation, not re-walked on every access. A
    // bare EntryName isn't enough here — see "Validation questions —
    // answered", #1 — two different submodules can each have their own
    // same-named test.
    pub target: LogicalPath,
}
```

Code that wants the actual `LogicalTest` looks it up by `target` —
walking `target.modules` through nested `LogicalModule.modules` maps,
then indexing the leaf module's `tests` map by `target.name`. Still cheap
(one `BTreeMap` lookup per path segment), and it keeps `LogicalRequirement`
from borrowing from `LogicalModule` (which Rust would otherwise make
painful).

## Draft vs. validated: two types, not one

This is the mechanism for "let the user make several changes in any order
without enforcing too many constraints, but require validation before
saving or doing whole-tree queries":

```rust
/// Freely mutable. No cross-entity invariant is enforced while editing —
/// a dependency can point at a name that doesn't exist yet, two edits in
/// flight can temporarily leave things inconsistent, etc. Only
/// `disk`-level structural rules that are checked per-field at
/// construction time still apply (e.g. you can't construct a `StatusV1`
/// that isn't one of the three variants — that's just the type system).
pub struct ProjectDraft {
    pub definition: RootV1,
    pub tree: ModuleDraft,
}

/// The result of successfully validating a `ProjectDraft`. Every
/// reference is resolved, every cross-entity rule holds. The only way to
/// get one is `ProjectDraft::validate`.
pub struct ValidatedProject(ProjectDraft /* + resolved reference data */);

impl ProjectDraft {
    pub fn validate(self) -> Result<ValidatedProject, ValidationErrors>;
}

impl ValidatedProject {
    /// Hands back a mutable draft — the only way to change a
    /// `ValidatedProject` is to go back through validation.
    pub fn into_draft(self) -> ProjectDraft;

    pub fn save(&self, fs: &dyn Filesystem, git: &dyn Git, dir: &Path) -> Result<(), SaveError>;
    pub fn dependency_chain(&self, requirement: &EntryName) -> Vec<&EntryName>;
    pub fn is_requirement_met(&self, requirement: &EntryName) -> bool;
    // ... other whole-tree queries.
}
```

`ProjectDraft` (and its per-module `ModuleDraft`) is where every `add_*`/
`remove_*` operation lives — see below. None of them can fail on
cross-entity grounds (there's nothing to check yet); the only failure
modes are purely local ones a `disk`-level type already rules out
(duplicate name in the same `BTreeMap`, say — represented as `Result`,
not a panic).

**Why not a single type with a `dirty` flag, or phantom-typed
`Project<State>`?** A flag is easy to forget to check. A phantom-typed
`Project<Draft>`/`Project<Validated>` gets the same safety with one type
instead of two, at the cost of every field/method needing to either work
for both states or be gated by `where State: ...` bounds — more type
machinery for not much benefit here, since the *shape* actually differs
too (`ValidatedProject` carries resolved reference data `ProjectDraft`
doesn't have). Two plain types, with `validate`/`into_draft` as the only
bridge, seems like the simplest thing that actually enforces the
separation the way the task wants: something reference-holding methods
like `dependency_chain` can require `&ValidatedProject` at the type level,
not the caller's discipline.

## Operations catalog

All of these live on `ProjectDraft`/`ModuleDraft` (a submodule's own
requirements/tests/results/attachments/templates are edited the same way
as the root's, since `ModuleDraft` is the same type at every depth):

- `create_project(name) -> ProjectDraft` — the empty starting point.
- `add_module(name) -> Result<(), DuplicateNameError>` /
  `remove_module(name) -> Option<ModuleDraft>`
- `add_requirement(name, ...) -> Result<(), DuplicateNameError>` /
  `remove_requirement(name) -> Option<RequirementDraft>`
- `add_test(name, ...)` / `remove_test(name)`
- `add_result(name, ...)` / `remove_result(name)`
- `add_attachment(path, ...)` / `remove_attachment(path)` — for both a
  module's pool and a requirement/test/result's local pool.
- `add_template(path, ...)` / `remove_template(path)` — module pool and a
  test's local pool.

Every `add_*` only fails if the name/path is already taken in that exact
map — never because of what it references (a test can reference a
dependency by name before that dependency exists; that only becomes a
problem at `validate()` time). `remove_*` never fails; removing something
other entries still reference is allowed at draft time and becomes a
validation error, not a panic or a blocked operation, matching "don't
enforce too many constraints while editing."

## What validation checks

Everything `disk`'s README already lists as "deferred to a higher layer,"
plus whatever else turns out to matter once this is real:

- Every `TestReferenceKind`/`DependencyReferenceKind` resolves to an
  actual entry (in this module or, per the leading-slash convention, the
  project root — `disk`'s `ReferencePath` resolution rules apply here).
- Every `AttachmentReferenceKind`/`TemplateReferenceKind` resolves to an
  actual file in the right pool (local vs. module, per which variant).
- No dependency cycle.
- A `Template`-kind test's results' attachments actually cover the test's
  template files file-for-file (`disk`'s `ResultKindV1::Template` doc
  comment flags this as explicitly out of scope for `disk`).

### Validation questions — answered

Surfaced by actually working through the bullets above in detail, and now
resolved:

1. **Resolved cross-module references store a full, project-root-relative
   logical path — not a bare `EntryName`.** A leaf name alone isn't
   unique (two different submodules can each have their own
   `generic_test`), so a resolved target needs to say *which module*.
   Concretely:

   ```rust
   /// The location of a requirement/test/result, as the sequence of
   /// submodule names from the project root down to (but not including)
   /// its own containing module, plus its own name. Empty `modules` means
   /// "directly under the project root."
   pub struct LogicalPath {
       pub modules: Vec<EntryName>,
       pub name: EntryName,
   }
   ```

   `ResolvedTestReference::target` (and every other resolved reference)
   becomes a `LogicalPath`, computed once during validation by walking
   `disk`'s leading-slash/relative `ReferencePath` convention down to a
   concrete answer. Looking up the actual entity means walking
   `modules` through nested `LogicalModule.modules` maps, then indexing
   the leaf map by `name` — still just `BTreeMap` lookups, just one per
   path segment instead of one flat lookup.

2. **Yes — remote dependency resolution happens over the network, at
   `validate()` time.** A `RemoteReferenceV1` gets resolved eagerly via
   `syscalls::RemoteGit`, same as everything else; an unreachable remote
   is a normal `ValidationError`, not a special non-fatal category. (This
   does mean `validate()` can fail for reasons external to the project's
   own content — accepted, not treated as a problem to design around.)

3. **The bare `Submodules` dependency is satisfied when every requirement
   in the module's entire submodule subtree is met.** "All submodules of
   a module" reads as the whole transitive subtree (submodules can nest
   to any depth, per `disk`'s README), not just direct children — so this
   recurses through every nested submodule, checking each of *their*
   requirements against the test/result-based "met" definition from
   "Requirement-met semantics" above. (Flagging this as my reading of
   "all submodules" rather than something spelled out with a depth
   limit — direct-children-only would be the alternative if transitive
   turns out to be wrong.) This is a `dependency_chain`/"is this
   dependency satisfied" concept, evaluated the same way "met" is —
   query-time, not a `validate()` rule (nothing about an unmet submodule
   requirement makes the *project* invalid to save).

4. **The test's declared `template`/`templates` references are what
   count for `Template`-kind coverage — local walk alone isn't enough,**
   since a test can now name module-level template files too. In
   practice, once validation question 4.1 (below) holds, the declared
   local references and the physically-walked local `template/` folder
   are the *same* set by construction — the resolved reference list is
   simply the authoritative one, because it's also the only one that can
   include module-level entries.

   4.1. **New rule: for every entity's local `attachments/` folder (and a
   test's local `template/` folder), the declared references and the
   physically-present files must match exactly.** Every file physically
   in a local pool must have a corresponding `Local*ReferenceV1` entry in
   that entity's RON file, and every declared local reference must
   resolve to a real file — extra, undeclared files *or* references
   naming a file that isn't there both fail validation. Scope is local
   pools only (`requirements/<stage>/attachments/`, `tests/<name>/
   attachments/`, `tests/<name>/template/`, `results/<stage>/
   attachments/`) — module-level `attachments/`/`templates/` pools are
   *not* subject to this, since they're a shared resource multiple
   entities can each reference a different subset of; requiring every
   module-level file to be referenced from everywhere wouldn't make
   sense.

5. **Yes — names and paths get sanitized.** `add_requirement`/
   `add_test`/`add_attachment`/etc. reject unsafe `EntryName`/`path`
   values at call time (empty, containing `/` or `..`, OS-reserved names,
   ...) rather than letting them reach `disk::save_*` and surface as a
   confusing IO error or, worse, an unintended path. This fits the
   operations catalog's existing shape — an `add_*` call can already fail
   locally (duplicate name); illegal name/path is just another local
   failure mode, not a cross-entity `validate()` concern.

6. **Not moot — the rule still holds, just at a different boundary.**
   Collapsing `test`/`tests` etc. into one `Vec` means `logical` itself
   carries no concept of "singular vs. plural form" and so cannot
   re-check "did the author set both" (there's only one field to check).
   But the *rule* — a `disk::RequirementDefinitionV1` must never have
   both `test` and `tests` set — still has to hold in whatever gets
   saved. That's the export step's job: converting a `LogicalRequirement`'s
   `Vec<ResolvedTestReference>` back into `disk`'s
   `Option<TestReferenceKind>`/`Option<NonEmptyVec<TestReferenceKind>>`
   always picks exactly one (singular for a one-element `Vec`, plural
   otherwise) by construction — there's no code path that could set both,
   so the rule is satisfied without `logical` ever needing to "know" it
   exists as a distinct concept.

### Cascading validation errors — B and C resolved, general rule

**B and C both resolve to the same rule: group by the missing/broken
target, not by each thing that references it.** One `ValidationError`
per unresolved target, carrying the list of everything that references
it — not one error per referencer. Concretely:

```rust
pub struct UnresolvedReferenceError {
    /// What's missing — a requirement, test, attachment file, etc.
    pub target: LogicalPath, // or PathBuf for attachments/templates
    /// Everything that references `target`, all reported together.
    pub referenced_by: Vec<LogicalPath>,
}
```

**C. A deleted dependency target.** Requirement `R` is deleted; three
other requirements each depended on it. One
`UnresolvedReferenceError { target: R, referenced_by: [A, B, C] }`,
not three separate errors.

**B. One missing template file, many results.** Test `T`'s `spec.typ`
is missing from `template/`; five of `T`'s results all needed it for
their coverage check. One `UnresolvedReferenceError { target:
T/template/spec.typ, referenced_by: [result 1, ..., result 5] }` (plus,
separately, `T`'s own declared reference to `spec.typ` is itself one of
the five `referenced_by` entries, or a sixth — same shape either way),
not six.

This generalizes past B and C: *any* rule that's fundamentally "does X
reference something that doesn't exist" groups by the nonexistent X,
because the number of things pointing at a hole is a fact about the hole,
not about each pointer. Cycle detection isn't this shape (a cycle is a
property of a set of edges collectively, not one missing target several
things point at) — see below for how it fits in.

### A: cycles are allowed in a draft, caught at validation

**Decided: no eager rejection.** A `ProjectDraft` (and `add_dependency`/
whatever sets a dependency) is allowed to contain a cycle — cycle
detection is a `validate()`-time graph check over resolved local edges,
not an add-time refusal. "Forbid entirely" could mean two different
things; worth separating them before answering:

- Cycles are already never *valid* — "no dependency cycle" is a
  `validate()` rule, so a `ValidatedProject` can never contain one. That
  part's already true regardless of what's decided here.
- The open question is whether a `ProjectDraft` should be able to
  *momentarily hold* a cycle-forming edge at all, by having the
  edge-setting operation refuse it up front (eager), vs. letting the
  draft hold anything and catching cycles only at `validate()` (lazy, the
  current plan).

Eager rejection has real costs that outweigh what it buys:

1. **It requires resolution — including network access — inside an
   operation that's supposed to be free.** Checking "would setting this
   dependency close a cycle" means resolving the new edge's target (and
   walking the existing graph from there) *at `add_dependency` time*. If
   the new edge is a `RemoteReferenceV1`, that resolution needs
   `syscalls::RemoteGit` — a network call — inside what the operations
   catalog currently promises is a cheap, always-local, never-fails-on-
   cross-entity-grounds call. That promise is why editing can happen "in
   any order" cheaply in the first place; an eager cycle check breaks it
   specifically for dependencies.

2. **It doesn't remove the need for the lazy check anyway.** A project
   can arrive already cyclic without ever going through `logical`'s
   `add_dependency` at all — `disk::load_project` on a directory that
   was hand-edited (someone directly wrote two `requirement.ron` files
   that depend on each other) builds a `ProjectDraft` straight from disk,
   bypassing every `add_*` operation. `validate()` still has to catch
   that case. So eager rejection would be pure *additional* complexity
   layered on top of a check that has to exist regardless, not a
   replacement for it.

3. **It constrains edit order for legitimate restructuring, contradicting
   "edit in any order."** Reversing a dependency — requirement `A`
   currently depends on `B`, and the author wants it the other way
   round — means the draft needs to go through *some* sequence of
   add/remove calls. If eager rejection is in place, `add_dependency(A,
   B)` while `A → B` still exists is fine (no new edge, or already
   handled some other way), but adding the reverse edge `B → A` *before*
   removing `A → B` would momentarily close a 2-cycle and get rejected —
   forcing "remove old edge, then add new edge," never the reverse order,
   for this one kind of edit. Every other edit in this design is
   order-independent; this would be the one exception.

   (Pure *addition* of a whole new acyclic dependency graph, one edge at
   a time, never has this problem regardless of order — a subset of a
   cycle's edges is never itself a cycle, so eager rejection would only
   ever fire on a genuinely cycle-closing edge. The restructuring case
   above is the specific situation where it bites.)

4. **Cross-repository cycles are effectively out of scope either way, so
   "forbid entirely" can't actually mean "forbid all cycles" in the
   fully general case.** A `RemoteReferenceV1` dependency points at a
   requirement in a *different* git repository. Detecting a cycle that
   passes through one requires knowing that remote repo's own dependency
   graph — which might itself depend on a third repo, and so on. Fully
   checking this means recursively cloning and parsing an unbounded chain
   of remote repositories on every validate (or every eager add), which
   is expensive, can fail for reasons that have nothing to do with this
   project (a remote being down, per question 2 above), and can go stale
   the moment any remote changes after the check ran. The realistic scope
   for cycle detection — eager or lazy — is *local* dependency edges
   only; a remote reference is a leaf for this purpose. So even a
   from-scratch "forbid outright" design can't promise no cycles ever,
   only no *locally-detectable* cycles — which is exactly what the lazy,
   `validate()`-time graph check already provides, without the added
   cost and ordering constraint above.

**How this resolves the original scenario A** (an unresolved edge that
coincides with a real cycle among other edges): cycle detection walks
only *resolved* local edges. An edge that fails to resolve is reported
once via the grouped `UnresolvedReferenceError` above (its target is the
unresolved thing, its `referenced_by` includes the requirement holding
the broken edge) and is simply absent from the graph cycle detection
walks — never treated as "points nowhere" in a way that could fabricate
a false cycle, and never silently allowed to hide a real cycle among the
*other*, successfully-resolved edges (that cycle, if it doesn't need the
broken edge to exist, is still found and reported independently).

### Requirement-met semantics

**Decided, fully specified:** a requirement is met when it has one or
more tests, and *every one* of them satisfies both of the following —
fail either, for any one test, and the whole requirement is not met:

1. **The reference is current.** The requirement's own reference to that
   test — the `commit` inside the `LocalGitReference` in its `test`/
   `tests` field — equals that test's *currently computed* commit
   (`TestOnDisk::commit`, freshly walked at load time, not whatever was
   true when the reference was last written). If the requirement is
   pointing at a stale version of the test, this fails.
2. **A current, passing result exists.** Some result's
   `requirement_commit` equals the requirement's own currently computed
   commit (`RequirementOnDisk::commit`) *and* its `test_commit` equals
   that same test's currently computed commit *and* its `status` is
   `Pass`.

**Historical results are not errors, and don't count.** A `result.ron`
whose `requirement_commit`/`test_commit` name an older commit than what's
currently computed is a perfectly valid thing to have on disk — nothing
about condition 2 makes it invalid, it's simply excluded from the
`is_requirement_met` computation because it doesn't match *today's*
commits. (This is a query-time filter, not a `validate()` rule — see
below.) The same goes for a result with `status: Fail` or `Incomplete`
sitting at the current commit: present, valid, just not counted.

This is also the reason `include_attachments_in_commit`/
`include_template_in_commit` (the opt-out bools added to
`RequirementDefinitionV1`/`TestV1` earlier) matter here: since "met"
hinges on exact commit equality against `RequirementOnDisk::commit`/
`TestOnDisk::commit`, a project that wants editing `attachments/`/
`template/` to *not* invalidate an already-passing "met" status needs
those excluded from the computed commit — otherwise touching an
attachment would silently flip a requirement from met to unmet without
any content relevant to satisfying it having changed.

**This check belongs in `is_requirement_met`, not `validate()`.**
Everything it inspects — reference staleness, commit mismatches, result
status — describes normal states a project can validly sit in (a result
waiting to be re-run after a test changed is not an authoring error), so
none of it becomes a `ValidationError`. `validate()` only needs to
confirm each `TestReferenceKind`/result reference *resolves* to something
that exists (see "What validation checks" above); whether the commits
line up is `is_requirement_met`'s job, evaluated fresh against already-
resolved data, using no caching (consistent with "always recompute,"
decision 4 below).

The `disk`-level format change this needed has been implemented:
`ResultsV1`'s old single `path`/`commit` pair (renamed
`requirement_path`/`requirement_commit`) now sits alongside an
independent `test_path`/`test_commit` pair, so a result can name both the
requirement and the test it's for.

**Validation collects every error, it doesn't stop at the first one** —
deliberately different from `disk`'s per-function `Result<T, E>` style
(one failure, one file, stop). The point of validating a whole draft is to
hand the editor back a complete list of what's wrong so they can fix
several things before trying again, not play whack-a-mole one error at a
time. So `validate()` returns `Result<ValidatedProject,
Vec<ValidationError>>` (or a dedicated `ValidationErrors` newtype), not a
single early-exit error.

## Converting to/from `disk`

- **Import**: `disk::load_project` gives a `disk::ProjectOnDisk`
  (`Vec`-based, references unresolved). Converting that into a
  `ProjectDraft` is infallible reshaping (`Vec` → `BTreeMap`, dropping
  nothing) — except that two entries sharing a directory name would be
  impossible reaching this point anyway, since they'd already collide as
  filesystem paths before `disk` ever loaded them.
- **Export**: only `ValidatedProject` can be saved — `save()` reshapes
  back to `disk`'s `Vec`/`*OnDisk` types (`BTreeMap::values().cloned()`)
  and calls `disk::save_project`. A `ProjectDraft` has no `save()` at all;
  the type system is the enforcement mechanism for "validate before
  saving," not a runtime check.
- `logical`'s `Cargo.toml` needs a `disk = { path = "../disk" }`
  dependency added — not there yet.

## Decisions made

1. **Collapse singular/plural into one `Vec`.** Not mirroring `disk`'s
   `Option<T>`/`Option<NonEmptyVec<T>>` split — see the data model section
   above for the accepted RON-round-trip trade-off.
2. **"Met" = every one of a requirement's tests has a current, `Pass`
   result, and the requirement's reference to that test is itself
   current.** Fully specified under "Requirement-met semantics" above,
   including that this is a query-time check, not a `validate()` rule.
   The `disk`-level `ResultsV1` change this needed (two path/commit pairs
   instead of one) is implemented.
3. **`ValidationError`: one variant per rule**, each carrying enough
   context (which entity, which field) to report well — confirmed, exact
   variant list still to be filled in as validation rules get implemented.
4. **Always recompute reference resolution on `validate()`** — no caching
   across an `into_draft()`/`validate()` round-trip. Revisit only if
   profiling at real project sizes says otherwise.
5. **Resolved references carry a full `LogicalPath`, not a bare
   `EntryName`.** See "Validation questions — answered" #1.
6. **Remote dependencies resolve over the network, eagerly, inside
   `validate()`.** See #2 — an unreachable remote is a normal
   `ValidationError`.
7. **`Submodules` dependency = every requirement in the entire (transitive)
   submodule subtree is met.** See #3 — a query-time check, same as "met"
   itself, not a `validate()` rule.
8. **The `Template`-kind coverage check uses a test's declared
   `template`/`templates` references**, and every entity's local
   attachments/template pool must exactly match its declared local
   references (rule 4.1) — no undeclared physical files, no references to
   files that aren't there. See #4.
9. **Names and paths are sanitized, rejected at `add_*` call time.** See
   #5.
10. **The singular/plural mutual-exclusion rule isn't dropped** — it's
    enforced by construction at the `logical` → `disk` export step, not
    re-checked inside `logical`. See #6.
11. **Unresolved-reference errors group by the missing target, not by
    each referencer** — one `UnresolvedReferenceError { target,
    referenced_by }` per hole, not one error per thing pointing at it.
    See "Cascading validation errors" above.
12. **A draft is allowed to contain cycles; cycle detection stays a
    `validate()`-time graph check over resolved local edges, not an eager
    rejection inside the operation that sets a dependency.** See "A:
    cycles are allowed in a draft, caught at validation" above for the
    four reasons (network access inside a supposedly-cheap operation,
    doesn't remove the need for the lazy check anyway, constrains edit
    order for reversing a dependency, and cross-repo cycles are out of
    scope regardless).

## Still open

- The full `ValidationError` variant list (only a few rules are named so
  far under "What validation checks") — now includes the grouped
  `UnresolvedReferenceError` shape from "Cascading validation errors"
  above; the rest still need filling in.