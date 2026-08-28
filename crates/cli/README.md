# `cli`

A small human-usable command-line front end over the `logical` crate. It
exists for two reasons: letting a person drive the library by hand (create a
project, add requirements/tests/results, validate, check whether a
requirement is met, walk a dependency chain), and — more importantly for how
this crate is built — giving `logical` an in-process, string-args-in /
string-output-out entry point that test code can call directly, so the CLI's
own command-parsing and dispatch logic is itself covered by
`cargo llvm-cov --branch` like any other function.

## Architecture: `run()`, not `main()`

All logic lives behind one function:

```rust
pub fn run(
    args: impl IntoIterator<Item = String>,
    fs: &dyn syscalls::Filesystem,
    git: &dyn syscalls::Git,
    remote_git: &dyn syscalls::RemoteGit,
) -> Result<String, Error>
```

`main()` is two lines: collect `std::env::args()`, call `run` with the real
`StdFilesystem`/`SystemGit`/`SystemGit`, print the `Ok(String)` or the
`Err(Error)`'s `Display`, and set the process exit code.

Everything else — argument parsing (`clap::Parser`), loading/saving the
project via `disk`, mutating a `logical::draft::ProjectDraft`, validating —
happens inside `run` and the functions it calls. Because the filesystem/git
dependencies are passed in as trait objects rather than hardcoded, a test can
call `run` with a temp directory and fake or fault-injecting `syscalls`
implementations, entirely in-process: no subprocess, no stdin/stdout
plumbing, no special coverage tooling. `cargo llvm-cov` instruments `run`
exactly like it instruments any other function, because that's all it is.

Each invocation of `run` is one command against one on-disk project
directory (`--dir`). State isn't held in memory between invocations — every
mutating command loads the project fresh via `disk::load_project` +
`logical::convert::import_project`, applies one change, validates, and saves
back via `logical::convert::export_project` + `disk::save_project`. This
mirrors how a real user would actually drive the tool (repeated CLI
invocations from a shell), and it means every command is a complete,
independently testable round trip through `disk` and `logical` together.

## Commands

| Command | What it does |
|---|---|
| `create-project --dir <dir> --name <name>` | Creates a fresh project at `dir` (empty tree, trivially valid) and saves it. |
| `add-module --dir <dir> --module <path> --name <name>` | Adds a submodule under the module at `path` (`""` = project root, `a/b` = nested). |
| `add-requirement --dir <dir> --module <path> --name <name> --title <title>` | Adds a requirement stage. |
| `add-test --dir <dir> --module <path> --name <name> --title <title> --result-kind <free-form\|template>` | Adds a test stage. |
| `add-result --dir <dir> --module <path> --name <name> --title <title> --requirement-path <p> --requirement-commit <c> --test-path <p> --test-commit <c> --status <pass\|fail\|incomplete>` | Adds a result. |
| `link-test --dir <dir> --module <path> --requirement <name> --test-path <p> --commit <c>` | Adds a local test reference to a requirement's `tests`. |
| `validate --dir <dir>` | Loads and validates; prints `ok` or every `ValidationError`, one per line. |
| `is-met --dir <dir> --module <path> --requirement <name>` | Validates, then reports whether that requirement is met. |
| `dependency-chain --dir <dir> --module <path> --requirement <name>` | Validates, then prints the transitive local dependency chain. |
| `is-complete --dir <dir> --module <path>` | Validates, then reports whether every requirement in `<path>`'s subtree is met (the whole project when `<path>` is omitted). |

`<path>` for `--module` is a `/`-separated list of submodule names from the
project root (empty string for the root itself) — the same shape as
`logical::LogicalPath::modules`, just written as a flat string since it's
coming from a shell argument rather than a `Vec<EntryName>`.

This is deliberately not a complete surface over every field `logical`
supports (dependency edges beyond `link-test`, remote references, guidance
text, attachments/templates) — it covers enough of the operations catalog to
build and validate a real project shape end-to-end, and to exercise the
`logical` error paths worth testing from the outside. Extending it to more
fields is straightforward and follows the same pattern.

## Testing philosophy: three ways to get a project onto disk

### 1. Self-hosted fixtures (preferred for anything reachable through `run`)

Build the on-disk state by calling `run` itself, repeatedly, against a fresh
temp directory — the same sequence of commands a real user would type:

```rust
let dir = fresh_temp_dir("happy-path");
run(["cli", "--dir", &dir, "create-project", "--name", "Demo"], ...).unwrap();
run(["cli", "--dir", &dir, "add-requirement", "--module", "", "--name", "definition", "--title", "Definition"], ...).unwrap();
// ...
```

This is the right default: it proves the CLI's own argument handling works,
it proves `logical`'s draft/export/validate/save round trip works, and it
never risks the fixture and the code drifting apart the way a hand-maintained
`.ron` file can.

### 2. Hand-written on-disk fixtures (for errors `run` itself can't produce)

Some error paths only exist on the *load* side: malformed RON, a dangling
reference nothing in `logical`'s public API can construct, an
undeclared/extra file in an `attachments/`/`template/` folder, a
`requirement.ron` hand-edited to set both `test` and `tests`. These have to
be written directly to disk, bypassing `run` entirely, then read back by
pointing a command at that directory.

Convention (matches `disk`'s own test style — see e.g.
`crates/disk/src/result/operations/save.rs`'s
`round_trips_a_result_through_a_tempdir`): a uniquely-named directory under
`std::env::temp_dir()`, built with the process id and `line!()` so parallel
`cargo test` runs and repeated test functions in the same file never
collide, written with plain `std::fs::create_dir_all`/`std::fs::write`, and
removed at the end of the test regardless of pass/fail:

```rust
fn fresh_temp_dir(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cli-{case}-{}-{}",
        std::process::id(),
        line!(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn validate_reports_a_dangling_test_reference() {
    let dir = fresh_temp_dir("dangling-test-ref");

    // Hand-write just enough of the on-disk shape to be loadable but
    // invalid: a project + one requirement referencing a test that was
    // never created.
    std::fs::write(dir.join("project.ron"), r#"RootV1(name: "Demo")"#).unwrap();
    let req_dir = dir.join("requirements/definition");
    std::fs::create_dir_all(&req_dir).unwrap();
    std::fs::write(
        req_dir.join("requirement.ron"),
        r#"RequirementDefinitionV1(
            title: "Definition",
            test: Some(TestReferenceV1((path: "/tests/nonexistent", commit: "abc"))),
        )"#,
    ).unwrap();
    std::fs::write(req_dir.join("requirement.typ"), "").unwrap();

    let output = run(
        ["cli", "--dir", dir.to_str().unwrap(), "validate"],
        &StdFilesystem, &FixedGit, &FixedRemoteGit,
    ).unwrap();

    assert!(output.contains("does not exist"));
    std::fs::remove_dir_all(&dir).ok();
}
```

Use this style sparingly — only for the load-time/malformed-data corner that
genuinely can't be reached any other way. If a case *can* be built through
`run`, prefer style 1; it's less code to maintain and can't go stale against
`disk`'s RON shape.

### 3. IO-fault injection (for I/O error branches)

For "the disk write/read itself failed" branches (permission denied, etc.),
don't try to simulate real filesystem faults — pass
`syscalls::FaultInjectingFilesystem`/`FaultInjectingGit`/
`FaultInjectingRemoteGit` as the trait objects `run` is called with, and
inject a fault on the specific path the command is expected to touch. This
is the same mechanism `disk`'s own test suite uses (see
`syscalls::FaultInjectingFilesystem` and `crates/disk`'s
`reports_io_errors_saving_result_ron`).

## Measuring branch coverage

Because every command reduces to a plain function call (`run(args, fs, git,
remote_git) -> Result<String, Error>`), there's no subprocess or binary
instrumentation to worry about — tests are ordinary `#[test]` functions, so
the usual invocation covers them:

```sh
cargo +nightly llvm-cov --branch -p cli --show-missing-lines --summary-only
```

**The `Branches` column in that summary is close to meaningless here, and
has already hidden a real gap once.** Rust's coverage instrumentation only
treats `if`/`if-let`/boolean (`&&`/`||`) conditions as "branch regions" —
`match` arms and `?`-operator early-returns aren't tracked as branches at
all. `run_command`'s dispatch is almost entirely one `match Command { ... }`
with `?`-heavy bodies, so the file has only 2 true `if`-shaped branches
total; both being covered says nothing about whether every `match` arm or
every `?`'s error path was ever hit. Concretely: four near-identical
`ErrorKind::ModuleNotFound` closures (one per `match` arm) and
`LinkTest`'s `ErrorKind::RequirementNotFound` path sat at a literal
execution count of 0 while the branch summary still read "100%" — only
visible by generating the HTML report (`cargo +nightly llvm-cov --branch -p
cli --html`, then check `target/llvm-cov/html`) and inspecting the
per-region hit counts, since even `--show-missing-lines`' line-level view
missed it (the `?` shares its line with code that always runs). Read the
`Regions`/`Lines` columns and `Missed Regions`/`Missed Functions` instead —
they cover `match` arms and closures — and when those show a real crate-vs-
tool-limitation ambiguity, check the HTML report rather than trusting the
summary table.

If black-box coverage of the actual compiled binary (`main()`'s argv
handling, real process exit codes) is ever wanted on top of this, that needs
`cargo llvm-cov --branch` run at the *workspace* level (so it can set
`LLVM_PROFILE_FILE` for spawned child processes) with tests using
`assert_cmd::Command::cargo_bin("cli")`. That's out of scope here — `main()`
is intentionally two lines with nothing worth chasing coverage on, and
everything it calls is already covered via `run`.
