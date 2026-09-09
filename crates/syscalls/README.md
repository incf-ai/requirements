# syscalls

The bottom of the workspace: three narrow traits — `Filesystem`, `Git`,
`RemoteGit` — plus a real implementation of each and a set of
fault-injecting/fake wrappers used everywhere above this crate. Nothing in
`disk` or `logical` calls `std::fs` or shells out to `git` directly; both go
through these trait objects instead, which is what lets their tests inject
arbitrary IO/git failures instead of depending on real disk state or a real
repository.

## The traits

- **`Filesystem`** — `read_to_string`/`read`/`read_dir`/`is_dir`/`exists`/
  `write`/`create_dir_all`/`remove_dir_all`. `StdFilesystem` is the real
  implementation, a thin pass-through to `std::fs`.
- **`Git`** — commit lookups and working-tree operations against a local
  repository: `commit_for_path_excluding`/`commit_for_path`, `is_repository`,
  `init_repository`, `changed_paths`, `commit_all`, `diff`, `push`.
  `is_repository`/`init_repository`/`push` have default trait-level
  implementations (`true`/no-op/no-op success) so fakes that don't care about
  those don't need their own override — but a wrapper that overrides *any*
  `Git` method (see `FaultInjectingGit` below) still needs to explicitly
  delegate the rest to its inner `Git`, since a method the wrapper doesn't
  override falls back to the trait's own default rather than the inner
  value's behavior. `SystemGit` is the real implementation, shelling out to
  `git` via `std::process::Command`.
- **`RemoteGit`** — `commit_for_remote(url, path)`, the one thing `logical`'s
  `validate()` needs from a *remote* repository (resolving a
  `RemoteReferenceV1` dependency). `SystemGit` implements this too, via a
  throwaway local clone.

## Fault-injecting wrappers

Each trait has a matching wrapper — `FaultInjectingFilesystem<F>`,
`FaultInjectingGit<G>`, `FaultInjectingRemoteGit<G>` — that wraps a real (or
fake) inner implementation and lets a test force specific calls to fail
instead of depending on real IO/git failures, which are impractical to
trigger on demand (there's no reliable way to make a real `write()` hit
`StorageFull`, or a real `git push` hit a specific network error, from a
test). `.inject(path, io::ErrorKind)` registers a fault for one path;
`.clear(path)` removes it. Faults are keyed per-method by whichever `Path`
argument is the specific target of that call (e.g. `commit_for_path_excluding`
and `diff` key on the path being asked about, not the containing directory;
`changed_paths`/`commit_all`/`push`/`init_repository` key on the repository
root, since that's all they take). `io::ErrorKind` is arbitrary — every fault
injected anywhere in this workspace has historically been `PermissionDenied`,
but `StorageFull`/`Interrupted`/etc. work the same way and are exercised in a
handful of `disk`/`cli` tests as of this writing.

`is_repository` has no `Result` to carry an injected fault through, so
`FaultInjectingGit` instead reports an injected path as simply not a
repository (`false`) — the closest bool-shaped equivalent to "this failed."

## `SlowFilesystem`

Wraps a `Filesystem`, sleeping a configurable `Duration` before every
*mutating* call (`write`/`create_dir_all`/`remove_dir_all`) — reads stay
fast. Real filesystem operations against a small project are usually fast
enough (sub-millisecond, no network or subprocess involved, unlike `Git`) to
win a race against a deliberately short test timeout, which otherwise makes
a "the save is still in progress" state impossible to observe reliably.
Built specifically for `gui-ui`'s exit-dialog Saving/TimedOut tests (see that
crate's README) — reach for it whenever a test needs to hold a real save
open long enough to observe an in-progress state, rather than trying to time
a race against however fast the filesystem happens to be.

## Fakes used above this crate

Several crates define their own hand-written `Git`/`RemoteGit` fakes local
to their own test modules (`disk::test_support::FixedGit`,
`logical::test_support::FixedGit`, `cli`'s and `gui-core::actor`'s own test
modules) rather than sharing one from here — each returns a fixed commit
string (`"deadbeef"`) for every lookup, deterministic and independent of any
real `.git` state. They're duplicated per crate rather than centralized;
worth consolidating here eventually, but that's a larger cross-crate change,
not something this crate currently provides.

## Testing this crate

`SystemGit`'s own tests run against real scratch git repositories (`git
init`/`commit`/`push` against a real bare local remote under a
process-id-and-`line!()`-namespaced temp directory, removed at the end of
each test) rather than mocking the `git` binary itself — this is the one
crate where that's appropriate, since it's the thing every other crate's
fakes exist to avoid needing. The fault-injecting wrappers' own tests
instead confirm each method's fault path fires when injected and clears
when `.clear()`'d, using `SystemGit` as the inner implementation.
