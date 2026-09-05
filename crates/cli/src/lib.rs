use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use disk::{
    DependencyReferenceKind, EntryName, LocalGitReference, ReferencePath, RemoteGitReference,
    ResultKindV1, StatusV1, TestReferenceKind,
};
use logical::draft::{
    ModuleDraft, ProjectDraft, RequirementDraft, ResultDraft, TestDraft, create_project,
};
use logical::{LogicalPath, convert, validate};
use syscalls::{Filesystem, Git, RemoteGit};
use thiserror::Error;

/// See `crates/cli/README.md`, "Architecture": `main()` is a thin shim
/// around this. Everything is a plain function call — no subprocess, no
/// argv/stdout plumbing — so it's coverage-instrumented like any other
/// function.
#[derive(Parser)]
#[command(name = "cli")]
struct Cli {
    /// The project's root directory on disk.
    #[arg(long)]
    dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    CreateProject {
        #[arg(long)]
        name: String,
    },
    AddModule {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        name: String,
    },
    AddRequirement {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        title: String,
    },
    AddTest {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        title: String,
        #[arg(long, value_enum)]
        result_kind: ResultKindArg,
    },
    AddResult {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        requirement_path: String,
        #[arg(long)]
        requirement_commit: String,
        #[arg(long)]
        test_path: String,
        #[arg(long)]
        test_commit: String,
        #[arg(long, value_enum)]
        status: StatusArg,
    },
    LinkTest {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        requirement: String,
        #[arg(long)]
        test_path: String,
        #[arg(long)]
        commit: String,
    },
    LinkRemoteDependency {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        requirement: String,
        /// A `git`-clonable URL (`http(s)://`, `ssh://`, `file://`, or a
        /// plain local path — anything `git clone`/`git ls-remote` accept).
        #[arg(long)]
        url: String,
        /// Path within the remote repository; absent means the remote
        /// repository's own root.
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        commit: String,
    },
    Validate,
    IsMet {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        requirement: String,
    },
    DependencyChain {
        #[arg(long, default_value = "")]
        module: String,
        #[arg(long)]
        requirement: String,
    },
    /// Whether every requirement in `--module`'s subtree (the whole project
    /// when omitted) is met — see "Validation questions — answered" #3 in
    /// `crates/logical/README.md`: this is what a bare `Submodules`
    /// dependency evaluates against.
    IsComplete {
        #[arg(long, default_value = "")]
        module: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ResultKindArg {
    FreeForm,
    Template,
}

impl From<ResultKindArg> for ResultKindV1 {
    fn from(value: ResultKindArg) -> Self {
        match value {
            ResultKindArg::FreeForm => ResultKindV1::FreeForm,
            ResultKindArg::Template => ResultKindV1::Template,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Pass,
    Fail,
    Incomplete,
}

impl From<StatusArg> for StatusV1 {
    fn from(value: StatusArg) -> Self {
        match value {
            StatusArg::Pass => StatusV1::Pass,
            StatusArg::Fail => StatusV1::Fail,
            StatusArg::Incomplete => StatusV1::Incomplete,
        }
    }
}

#[derive(Debug, Error)]
enum ErrorKind {
    #[error("failed to parse arguments: {0}")]
    Parse(#[from] clap::Error),
    #[error("failed to load project: {0}")]
    Load(#[from] disk::project::operations::load::Error),
    #[error("failed to save project: {0}")]
    Save(#[from] disk::project::operations::save::Error),
    #[error("no module `{0}`")]
    ModuleNotFound(String),
    #[error("no requirement `{0}`")]
    RequirementNotFound(String),
    #[error(transparent)]
    AddNamedChild(#[from] logical::draft::AddNamedChildError),
    #[error("validation failed:\n{}", format_validation_errors(.0))]
    Validation(Vec<validate::ValidationError>),
}

fn format_validation_errors(errors: &[validate::ValidationError]) -> String {
    errors
        .iter()
        .map(|error| format!("- {error}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Opaque error type — see `crates/logical/README.md`'s error-type
/// convention (borrowed from `disk`): callers match on the message, not on
/// structure. Unlike `logical::validate::ValidationError`, nothing here
/// needs to be programmatically distinguished by another part of this
/// crate, so there's no reason to keep the variants public.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorKind);

/// Parses `args` as a `cli` invocation and runs it against `dir` (from the
/// parsed `--dir`), using the given `syscalls` implementations for all
/// filesystem/git access. See `crates/cli/README.md` for why this, and not
/// `main`, is the crate's real entry point.
pub fn run(
    args: impl IntoIterator<Item = String>,
    fs: &dyn Filesystem,
    git: &dyn Git,
    remote_git: &dyn RemoteGit,
) -> Result<String, Error> {
    let cli = Cli::try_parse_from(args).map_err(ErrorKind::from)?;
    run_command(&cli.dir, cli.command, fs, git, remote_git).map_err(Error::from)
}

fn run_command(
    dir: &std::path::Path,
    command: Command,
    fs: &dyn Filesystem,
    git: &dyn Git,
    remote_git: &dyn RemoteGit,
) -> Result<String, ErrorKind> {
    match command {
        Command::CreateProject { name } => {
            let draft = create_project(name);
            save_draft(fs, git, dir, &draft)?;
            Ok("created".to_string())
        }
        Command::AddModule { module, name } => {
            mutate(fs, git, dir, |draft| {
                let target = find_module_mut(&mut draft.tree, &module)
                    .ok_or_else(|| ErrorKind::ModuleNotFound(module.clone()))?;
                target.add_module(&name)?;
                Ok(())
            })?;
            Ok("added".to_string())
        }
        Command::AddRequirement {
            module,
            name,
            title,
        } => {
            mutate(fs, git, dir, |draft| {
                let target = find_module_mut(&mut draft.tree, &module)
                    .ok_or_else(|| ErrorKind::ModuleNotFound(module.clone()))?;
                target.add_requirement(&name, RequirementDraft::new(title))?;
                Ok(())
            })?;
            Ok("added".to_string())
        }
        Command::AddTest {
            module,
            name,
            title,
            result_kind,
        } => {
            mutate(fs, git, dir, |draft| {
                let target = find_module_mut(&mut draft.tree, &module)
                    .ok_or_else(|| ErrorKind::ModuleNotFound(module.clone()))?;
                target.add_test(&name, TestDraft::new(title, result_kind.into()))?;
                Ok(())
            })?;
            Ok("added".to_string())
        }
        Command::AddResult {
            module,
            name,
            title,
            requirement_path,
            requirement_commit,
            test_path,
            test_commit,
            status,
        } => {
            mutate(fs, git, dir, |draft| {
                let target = find_module_mut(&mut draft.tree, &module)
                    .ok_or_else(|| ErrorKind::ModuleNotFound(module.clone()))?;
                let mut result = ResultDraft::new(
                    title,
                    ReferencePath(requirement_path),
                    requirement_commit,
                    ReferencePath(test_path),
                    test_commit,
                );
                result.status = status.into();
                target.add_result(&name, result)?;
                Ok(())
            })?;
            Ok("added".to_string())
        }
        Command::LinkTest {
            module,
            requirement,
            test_path,
            commit,
        } => {
            mutate(fs, git, dir, |draft| {
                let target = find_module_mut(&mut draft.tree, &module)
                    .ok_or_else(|| ErrorKind::ModuleNotFound(module.clone()))?;
                let requirement = target
                    .requirements
                    .get_mut(&EntryName(requirement.clone()))
                    .ok_or(ErrorKind::RequirementNotFound(requirement))?;
                requirement
                    .tests
                    .push(TestReferenceKind::TestReferenceV1(LocalGitReference {
                        path: ReferencePath(test_path),
                        commit,
                    }));
                Ok(())
            })?;
            Ok("linked".to_string())
        }
        Command::LinkRemoteDependency {
            module,
            requirement,
            url,
            path,
            commit,
        } => {
            mutate(fs, git, dir, |draft| {
                let target = find_module_mut(&mut draft.tree, &module)
                    .ok_or_else(|| ErrorKind::ModuleNotFound(module.clone()))?;
                let requirement = target
                    .requirements
                    .get_mut(&EntryName(requirement.clone()))
                    .ok_or(ErrorKind::RequirementNotFound(requirement))?;
                requirement
                    .dependencies
                    .push(DependencyReferenceKind::RemoteReferenceV1(
                        RemoteGitReference {
                            url,
                            path: path.map(ReferencePath),
                            commit,
                        },
                    ));
                Ok(())
            })?;
            Ok("linked".to_string())
        }
        Command::Validate => {
            let draft = load(fs, git, dir)?;
            match validate::validate(draft, remote_git) {
                Ok(_) => Ok("ok".to_string()),
                Err(errors) => Err(ErrorKind::Validation(errors)),
            }
        }
        Command::IsMet {
            module,
            requirement,
        } => {
            let draft = load(fs, git, dir)?;
            let validated = validate::validate(draft, remote_git).map_err(ErrorKind::Validation)?;
            let path = logical_path(&module, &requirement);
            Ok(if validated.is_requirement_met(&path) {
                "met".to_string()
            } else {
                "not met".to_string()
            })
        }
        Command::DependencyChain {
            module,
            requirement,
        } => {
            let draft = load(fs, git, dir)?;
            let validated = validate::validate(draft, remote_git).map_err(ErrorKind::Validation)?;
            let path = logical_path(&module, &requirement);
            let chain = validated.dependency_chain(&path);
            Ok(chain
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "))
        }
        Command::IsComplete { module } => {
            let draft = load(fs, git, dir)?;
            let validated = validate::validate(draft, remote_git).map_err(ErrorKind::Validation)?;
            Ok(
                if validated.all_requirements_met_in_subtree(&module_path(&module)) {
                    "complete".to_string()
                } else {
                    "incomplete".to_string()
                },
            )
        }
    }
}

fn logical_path(module: &str, requirement: &str) -> LogicalPath {
    LogicalPath {
        modules: module_path(module),
        name: EntryName(requirement.to_string()),
    }
}

fn module_path(module: &str) -> Vec<EntryName> {
    module
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| EntryName(segment.to_string()))
        .collect()
}

fn find_module_mut<'a>(root: &'a mut ModuleDraft, module: &str) -> Option<&'a mut ModuleDraft> {
    let mut current = root;
    for segment in module_path(module) {
        current = current.modules.get_mut(&segment)?;
    }
    Some(current)
}

fn load(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &std::path::Path,
) -> Result<ProjectDraft, ErrorKind> {
    let on_disk = disk::load_project(fs, git, dir)?;
    Ok(convert::import_project(on_disk))
}

/// Persists a draft as-is, with no validation gate. See
/// `crates/logical/README.md`'s "Draft vs. validated": a draft is allowed
/// to be temporarily inconsistent between edits (a dangling reference, an
/// unmet requirement), and since each `cli` invocation is a separate
/// process, *something* has to persist that in-progress state to disk
/// between commands. `logical::ValidatedProject::save` intentionally can't
/// do this — it only ever wraps an already-validated draft — so this saves
/// via `disk::save_project` directly. Only the explicit `validate` (and
/// `is-met`/`dependency-chain`, which need a `ValidatedProject` to call
/// into) commands go through `validate::validate`.
fn save_draft(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &std::path::Path,
    draft: &ProjectDraft,
) -> Result<(), ErrorKind> {
    let on_disk = convert::export_project(draft);
    disk::save_project(fs, git, dir, &on_disk)?;
    Ok(())
}

fn mutate(
    fs: &dyn Filesystem,
    git: &dyn Git,
    dir: &std::path::Path,
    change: impl FnOnce(&mut ProjectDraft) -> Result<(), ErrorKind>,
) -> Result<(), ErrorKind> {
    let mut draft = load(fs, git, dir)?;
    change(&mut draft)?;
    save_draft(fs, git, dir, &draft)
}

#[cfg(test)]
mod test {
    use super::*;
    use syscalls::{ChangedPathsError, CommitAllError, CommitForPathError, CommitForRemoteError, StdFilesystem, SystemGit};

    struct FixedGit;
    impl Git for FixedGit {
        fn commit_for_path_excluding(
            &self,
            _path: &std::path::Path,
            _excludes: &[&std::path::Path],
        ) -> Result<String, CommitForPathError> {
            Ok("deadbeef".to_string())
        }

        fn changed_paths(&self, _dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, ChangedPathsError> {
            Ok(Vec::new())
        }

        fn commit_all(&self, _dir: &std::path::Path, _message: &str) -> Result<(), CommitAllError> {
            Ok(())
        }
    }

    struct FixedRemoteGit;
    impl RemoteGit for FixedRemoteGit {
        fn commit_for_remote(
            &self,
            _url: &str,
            _path: Option<&std::path::Path>,
        ) -> Result<String, CommitForRemoteError> {
            Ok("deadbeef".to_string())
        }
    }

    /// A real, local git repository, standing in for a remote one — see
    /// `crates/cli/README.md`, "Testing philosophy": exercising
    /// `syscalls::SystemGit` against `git clone`/`git ls-remote` on a
    /// filesystem path is weaker than testing against actual `http(s)://`/
    /// `ssh://` remotes (no network/auth/protocol edge cases), but it's the
    /// only variant of "a real remote" that's runnable in a test without
    /// network access, and it's enough to prove `logical::validate`'s
    /// `RemoteGit` wiring — request, success, and failure — actually works
    /// end to end rather than only against `FixedRemoteGit`.
    fn init_git_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("marker.txt"), "hello").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "init"]);
    }

    fn fresh_temp_dir(case: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cli-{case}-{}-{}", std::process::id(), line!(),));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(parts: &[&str]) -> Vec<String> {
        std::iter::once("cli".to_string())
            .chain(parts.iter().map(|s| s.to_string()))
            .collect()
    }

    fn run_ok(dir: &std::path::Path, parts: &[&str]) -> String {
        let dir_str = dir.to_str().unwrap().to_string();
        let mut full = vec!["--dir", &dir_str];
        full.extend_from_slice(parts);
        run(args(&full), &StdFilesystem, &FixedGit, &FixedRemoteGit).unwrap()
    }

    fn run_err(dir: &std::path::Path, parts: &[&str]) -> String {
        let dir_str = dir.to_str().unwrap().to_string();
        let mut full = vec!["--dir", &dir_str];
        full.extend_from_slice(parts);
        run(args(&full), &StdFilesystem, &FixedGit, &FixedRemoteGit)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn builds_and_validates_a_minimal_project() {
        let dir = fresh_temp_dir("minimal");
        let dir_str = dir.to_str().unwrap().to_string();

        assert_eq!(
            run_ok(&dir, &["create-project", "--name", "Demo"]),
            "created"
        );
        assert_eq!(
            run_ok(
                &dir,
                &[
                    "add-requirement",
                    "--name",
                    "definition",
                    "--title",
                    "Definition"
                ]
            ),
            "added"
        );
        assert_eq!(
            run_ok(
                &dir,
                &[
                    "add-test",
                    "--name",
                    "generic_test",
                    "--title",
                    "Generic Test",
                    "--result-kind",
                    "free-form"
                ]
            ),
            "added"
        );
        assert_eq!(
            run_ok(
                &dir,
                &[
                    "link-test",
                    "--requirement",
                    "definition",
                    "--test-path",
                    "/tests/generic_test",
                    "--commit",
                    "deadbeef"
                ]
            ),
            "linked"
        );
        assert_eq!(
            run_ok(
                &dir,
                &[
                    "add-result",
                    "--name",
                    "definition",
                    "--title",
                    "Definition",
                    "--requirement-path",
                    "/requirements/definition",
                    "--requirement-commit",
                    "deadbeef",
                    "--test-path",
                    "/tests/generic_test",
                    "--test-commit",
                    "deadbeef",
                    "--status",
                    "pass"
                ]
            ),
            "added"
        );

        assert_eq!(run_ok(&dir, &["validate"]), "ok");
        assert_eq!(
            run_ok(&dir, &["is-met", "--requirement", "definition"]),
            "met"
        );
        assert_eq!(
            run_ok(&dir, &["dependency-chain", "--requirement", "definition"]),
            ""
        );
        assert_eq!(run_ok(&dir, &["is-complete"]), "complete");

        let _ = dir_str;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_test_and_result_accept_every_result_kind_and_status_arg() {
        let dir = fresh_temp_dir("kinds-and-statuses");
        run_ok(&dir, &["create-project", "--name", "Demo"]);

        run_ok(
            &dir,
            &[
                "add-test",
                "--name",
                "templated_test",
                "--title",
                "Templated Test",
                "--result-kind",
                "template",
            ],
        );
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );
        for (name, status) in [("failing", "fail"), ("incomplete", "incomplete")] {
            assert_eq!(
                run_ok(
                    &dir,
                    &[
                        "add-result",
                        "--name",
                        name,
                        "--title",
                        "Title",
                        "--requirement-path",
                        "/requirements/definition",
                        "--requirement-commit",
                        "deadbeef",
                        "--test-path",
                        "/tests/templated_test",
                        "--test-commit",
                        "deadbeef",
                        "--status",
                        status,
                    ]
                ),
                "added"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_module_then_add_requirement_inside_it() {
        let dir = fresh_temp_dir("nested-module");

        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(&dir, &["add-module", "--name", "embeddings"]);
        assert_eq!(
            run_ok(
                &dir,
                &[
                    "add-requirement",
                    "--module",
                    "embeddings",
                    "--name",
                    "definition",
                    "--title",
                    "Definition"
                ]
            ),
            "added"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_requirement_in_a_nonexistent_module_is_an_error() {
        let dir = fresh_temp_dir("missing-module");
        run_ok(&dir, &["create-project", "--name", "Demo"]);

        let dir_str = dir.to_str().unwrap();
        let err = run(
            args(&[
                "--dir",
                dir_str,
                "add-requirement",
                "--module",
                "nonexistent",
                "--name",
                "definition",
                "--title",
                "Definition",
            ]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no module"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `AddRequirement`'s "module not found" branch is covered above — this
    /// covers the same `ModuleNotFound` error but reached through every
    /// *other* command that also takes a `--module`. Each `match Command`
    /// arm has its own copy of the `find_module_mut(...).ok_or_else(...)`
    /// closure (`match` arms aren't shared code the way a generic function
    /// is), so one arm being tested doesn't exercise the others — confirmed
    /// missing via `cargo llvm-cov --branch -p cli --html`, which showed
    /// these four closures at a literal 0 execution count despite the
    /// crate's reported "100%" *branch* coverage (a `match` arm isn't a
    /// `Branch Region` in rustc's coverage model, so a summary that only
    /// reads the Branches column can't see this gap at all).
    #[test]
    fn every_module_scoped_command_reports_a_missing_module() {
        let dir = fresh_temp_dir("missing-module-every-command");
        run_ok(&dir, &["create-project", "--name", "Demo"]);

        for parts in [
            &["add-module", "--module", "nonexistent", "--name", "x"][..],
            &[
                "add-test",
                "--module",
                "nonexistent",
                "--name",
                "x",
                "--title",
                "X",
                "--result-kind",
                "free-form",
            ][..],
            &[
                "add-result",
                "--module",
                "nonexistent",
                "--name",
                "x",
                "--title",
                "X",
                "--requirement-path",
                "/requirements/x",
                "--requirement-commit",
                "deadbeef",
                "--test-path",
                "/tests/x",
                "--test-commit",
                "deadbeef",
                "--status",
                "pass",
            ][..],
            &[
                "link-test",
                "--module",
                "nonexistent",
                "--requirement",
                "x",
                "--test-path",
                "/tests/x",
                "--commit",
                "deadbeef",
            ][..],
            &[
                "link-remote-dependency",
                "--module",
                "nonexistent",
                "--requirement",
                "x",
                "--url",
                "https://example.invalid/does-not-matter",
                "--commit",
                "unchecked",
            ][..],
        ] {
            assert!(run_err(&dir, parts).contains("no module"), "{parts:?}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn link_test_against_a_nonexistent_requirement_is_an_error() {
        let dir = fresh_temp_dir("link-test-missing-requirement");
        run_ok(&dir, &["create-project", "--name", "Demo"]);

        let err = run_err(
            &dir,
            &[
                "link-test",
                "--requirement",
                "nonexistent",
                "--test-path",
                "/tests/generic_test",
                "--commit",
                "deadbeef",
            ],
        );
        assert!(err.contains("no requirement"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn link_remote_dependency_against_a_nonexistent_requirement_is_an_error() {
        let dir = fresh_temp_dir("link-remote-dep-missing-requirement");
        run_ok(&dir, &["create-project", "--name", "Demo"]);

        let err = run_err(
            &dir,
            &[
                "link-remote-dependency",
                "--requirement",
                "nonexistent",
                "--url",
                "https://example.invalid/does-not-matter",
                "--commit",
                "unchecked",
            ],
        );
        assert!(err.contains("no requirement"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn link_remote_dependency_resolves_against_a_fake_remote_git() {
        // Fast path, no real `git` subprocess — exercises the same
        // `RemoteReferenceV1` resolution code in `validate::resolve` as the
        // real-repo tests below, just with `FixedRemoteGit` standing in.
        let dir = fresh_temp_dir("remote-dependency-fake");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );
        run_ok(
            &dir,
            &[
                "link-remote-dependency",
                "--requirement",
                "definition",
                "--url",
                "https://example.invalid/does-not-matter",
                "--commit",
                "unchecked",
            ],
        );

        assert_eq!(run_ok(&dir, &["validate"]), "ok");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn link_remote_dependency_against_a_real_local_git_repo_resolves() {
        let remote_dir = fresh_temp_dir("remote-repo-head");
        init_git_repo(&remote_dir);
        let remote_url = remote_dir.to_str().unwrap().to_string();

        let dir = fresh_temp_dir("remote-dependency-head");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );

        let dir_str = dir.to_str().unwrap().to_string();
        let linked = run(
            args(&[
                "--dir",
                &dir_str,
                "link-remote-dependency",
                "--requirement",
                "definition",
                "--url",
                &remote_url,
                "--commit",
                "unchecked",
            ]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap();
        assert_eq!(linked, "linked");

        // Validate against the *real* `SystemGit`, so `commit_for_remote`
        // actually shells out to `git ls-remote` on `remote_dir` — this is
        // the part `FixedRemoteGit` can't prove.
        let result = run(
            args(&["--dir", &dir_str, "validate"]),
            &StdFilesystem,
            &FixedGit,
            &SystemGit,
        )
        .unwrap();
        assert_eq!(result, "ok");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&remote_dir).ok();
    }

    #[test]
    fn link_remote_dependency_with_a_path_resolves_against_a_real_repo() {
        let remote_dir = fresh_temp_dir("remote-repo-path");
        init_git_repo(&remote_dir);
        let remote_url = remote_dir.to_str().unwrap().to_string();

        let dir = fresh_temp_dir("remote-dependency-path");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );

        let dir_str = dir.to_str().unwrap().to_string();
        run(
            args(&[
                "--dir",
                &dir_str,
                "link-remote-dependency",
                "--requirement",
                "definition",
                "--url",
                &remote_url,
                "--path",
                "marker.txt",
                "--commit",
                "unchecked",
            ]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap();

        let result = run(
            args(&["--dir", &dir_str, "validate"]),
            &StdFilesystem,
            &FixedGit,
            &SystemGit,
        )
        .unwrap();
        assert_eq!(result, "ok");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&remote_dir).ok();
    }

    #[test]
    fn link_remote_dependency_against_a_nonexistent_remote_fails_validation() {
        let dir = fresh_temp_dir("remote-dependency-missing");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );

        let dir_str = dir.to_str().unwrap().to_string();
        let missing_remote = dir.join("does-not-exist");
        run(
            args(&[
                "--dir",
                &dir_str,
                "link-remote-dependency",
                "--requirement",
                "definition",
                "--url",
                missing_remote.to_str().unwrap(),
                "--commit",
                "unchecked",
            ]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap();

        let err = run(
            args(&["--dir", &dir_str, "validate"]),
            &StdFilesystem,
            &FixedGit,
            &SystemGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to resolve"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_requirement_with_a_duplicate_name_is_an_error() {
        let dir = fresh_temp_dir("dup-requirement");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );

        let dir_str = dir.to_str().unwrap();
        let err = run(
            args(&[
                "--dir",
                dir_str,
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_module_with_a_duplicate_name_is_an_error() {
        let dir = fresh_temp_dir("dup-module");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(&dir, &["add-module", "--name", "embeddings"]);

        assert!(run_err(&dir, &["add-module", "--name", "embeddings"]).contains("already exists"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_test_with_a_duplicate_name_is_an_error() {
        let dir = fresh_temp_dir("dup-test");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-test",
                "--name",
                "generic_test",
                "--title",
                "Generic Test",
                "--result-kind",
                "free-form",
            ],
        );

        assert!(
            run_err(
                &dir,
                &[
                    "add-test",
                    "--name",
                    "generic_test",
                    "--title",
                    "Generic Test",
                    "--result-kind",
                    "free-form",
                ]
            )
            .contains("already exists")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_result_with_a_duplicate_name_is_an_error() {
        let dir = fresh_temp_dir("dup-result");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        let add_result_args = [
            "add-result",
            "--name",
            "definition",
            "--title",
            "Definition",
            "--requirement-path",
            "/requirements/definition",
            "--requirement-commit",
            "deadbeef",
            "--test-path",
            "/tests/generic_test",
            "--test-commit",
            "deadbeef",
            "--status",
            "pass",
        ];
        run_ok(&dir, &add_result_args);

        assert!(run_err(&dir, &add_result_args).contains("already exists"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_reports_an_unmet_requirement_with_no_tests() {
        let dir = fresh_temp_dir("unmet");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );

        assert_eq!(run_ok(&dir, &["validate"]), "ok");
        assert_eq!(
            run_ok(&dir, &["is-met", "--requirement", "definition"]),
            "not met"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_reports_an_unresolved_test_reference() {
        let dir = fresh_temp_dir("unresolved");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );
        run_ok(
            &dir,
            &[
                "link-test",
                "--requirement",
                "definition",
                "--test-path",
                "/tests/nonexistent",
                "--commit",
                "deadbeef",
            ],
        );

        let dir_str = dir.to_str().unwrap();
        let err = run(
            args(&["--dir", dir_str, "validate"]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_reports_a_load_error() {
        let dir =
            std::env::temp_dir().join(format!("cli-missing-{}-{}", std::process::id(), line!(),));

        let dir_str = dir.to_str().unwrap();
        let err = run(
            args(&["--dir", dir_str, "validate"]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to load project"));
    }

    #[test]
    fn unparseable_arguments_report_a_parse_error() {
        let err = run(
            args(&["--dir", "/tmp/whatever", "not-a-real-command"]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to parse arguments"));
    }

    /// Hand-written on-disk fixture — see `crates/cli/README.md`, "Testing
    /// philosophy," style 2: this shape (both `test` and `tests` set) can't
    /// be produced through `run` itself, since `logical`'s draft API only
    /// ever has one `Vec<TestReferenceKind>` field. It can only be reached
    /// by writing the RON directly.
    #[test]
    fn validate_reports_a_hand_written_ambiguous_requirement_definition() {
        let dir = fresh_temp_dir("ambiguous-ron");
        std::fs::write(dir.join("project.ron"), r#"RootV1(name: "Demo")"#).unwrap();

        let req_dir = dir.join("requirements/definition");
        std::fs::create_dir_all(&req_dir).unwrap();
        std::fs::write(
            req_dir.join("requirement.ron"),
            r#"RequirementDefinitionV1(
                title: "Definition",
                test: Some(TestReferenceV1((path: "/tests/generic_test", commit: "deadbeef"))),
                tests: Some([TestReferenceV1((path: "/tests/generic_test", commit: "deadbeef"))]),
            )"#,
        )
        .unwrap();
        std::fs::write(req_dir.join("requirement.typ"), "").unwrap();

        let dir_str = dir.to_str().unwrap();
        let err = run(
            args(&["--dir", dir_str, "validate"]),
            &StdFilesystem,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to load project"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_requirement_against_a_directory_with_no_project_reports_a_load_error() {
        // Covers `mutate`'s own `load(fs, git, dir)?` — distinct from
        // `Command::Validate`'s `load()?`, which `a_missing_directory_
        // reports_a_load_error` above already covers; `mutate` is a
        // separate call site with its own coverage region.
        let dir = fresh_temp_dir("mutate-missing-project");
        let err = run_err(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );
        assert!(err.contains("failed to load project"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_project_reports_io_errors_saving_project_ron() {
        use syscalls::FaultInjectingFilesystem;

        let dir = fresh_temp_dir("save-io-fault");
        let mut fs = FaultInjectingFilesystem::new(StdFilesystem);
        fs.inject(
            dir.join("project.ron"),
            std::io::ErrorKind::PermissionDenied,
        );

        let dir_str = dir.to_str().unwrap().to_string();
        let err = run(
            args(&["--dir", &dir_str, "create-project", "--name", "Demo"]),
            &fs,
            &FixedGit,
            &FixedRemoteGit,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to save project"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_met_against_a_directory_with_no_project_reports_a_load_error() {
        let dir = fresh_temp_dir("is-met-missing-project");
        let err = run_err(&dir, &["is-met", "--requirement", "definition"]);
        assert!(err.contains("failed to load project"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_chain_against_a_directory_with_no_project_reports_a_load_error() {
        let dir = fresh_temp_dir("dep-chain-missing-project");
        let err = run_err(&dir, &["dependency-chain", "--requirement", "definition"]);
        assert!(err.contains("failed to load project"));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn invalid_project_dir(case: &str) -> PathBuf {
        // A project that loads fine but fails `validate` — a requirement
        // referencing a test that was never created.
        let dir = fresh_temp_dir(case);
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );
        run_ok(
            &dir,
            &[
                "link-test",
                "--requirement",
                "definition",
                "--test-path",
                "/tests/nonexistent",
                "--commit",
                "deadbeef",
            ],
        );
        dir
    }

    #[test]
    fn is_met_reports_a_validation_error_from_an_invalid_project() {
        // Covers `Command::IsMet`'s own `validate::validate(...).map_err(...)?`
        // — a different call site from `Command::Validate`'s, which
        // `validate_reports_an_unresolved_test_reference` already covers.
        let dir = invalid_project_dir("is-met-invalid-project");
        let err = run_err(&dir, &["is-met", "--requirement", "definition"]);
        assert!(err.contains("does not exist"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_chain_reports_a_validation_error_from_an_invalid_project() {
        let dir = invalid_project_dir("dep-chain-invalid-project");
        let err = run_err(&dir, &["dependency-chain", "--requirement", "definition"]);
        assert!(err.contains("does not exist"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_complete_is_incomplete_when_any_requirement_is_unmet() {
        let dir = fresh_temp_dir("is-complete-incomplete");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );

        assert_eq!(run_ok(&dir, &["is-complete"]), "incomplete");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_complete_scopes_to_the_given_module() {
        let dir = fresh_temp_dir("is-complete-scoped");
        run_ok(&dir, &["create-project", "--name", "Demo"]);
        run_ok(&dir, &["add-module", "--name", "embeddings"]);
        // An unmet requirement outside `embeddings` shouldn't affect
        // `--module embeddings`'s own (vacuously complete, no requirements)
        // subtree.
        run_ok(
            &dir,
            &[
                "add-requirement",
                "--name",
                "definition",
                "--title",
                "Definition",
            ],
        );

        assert_eq!(run_ok(&dir, &["is-complete"]), "incomplete");
        assert_eq!(
            run_ok(&dir, &["is-complete", "--module", "embeddings"]),
            "complete"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_complete_against_a_directory_with_no_project_reports_a_load_error() {
        let dir = fresh_temp_dir("is-complete-missing-project");
        let err = run_err(&dir, &["is-complete"]);
        assert!(err.contains("failed to load project"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_complete_reports_a_validation_error_from_an_invalid_project() {
        let dir = invalid_project_dir("is-complete-invalid-project");
        let err = run_err(&dir, &["is-complete"]);
        assert!(err.contains("does not exist"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
