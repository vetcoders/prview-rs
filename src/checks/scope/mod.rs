//! Deciding how much test work a change actually requires.
//!
//! Contract: `~/AI_notes/10_projects/prview-rs/specs/2026-09-15_change-scoped-tests-contract.md`.
//! Its governing principle is that **change-scoped is not minimal-scoped**:
//! prview performs the full, honest review the change demands and only drops
//! work it can *prove* is irrelevant to that change. Scope answers "how much
//! must run"; the governor answers "how do we run it without frying the
//! machine"; the deadline answers "what happens when it does not fit". Mixing
//! those three is how a tool starts reporting green for work it never did.
//!
//! This module owns the first question only, and it owns it as a pure decision:
//! [`decide`] takes a captured [`ChangeSet`], the detected profile and — for
//! Rust — a workspace read from `cargo metadata`, and returns per ecosystem
//! either [`ScopeDecision::Full`] with a stated reason or
//! [`ScopeDecision::ChangeScoped`] with the selection. Every doubt resolves to
//! `Full`: an unknown file, an unmapped path, a failed metadata read, more than
//! one diff base. Nothing here runs a tool or builds a command line.
//!
//! **What this build does NOT do yet.** The checks still run their full
//! commands. The decision is computed and published so the machinery is
//! visible and reviewable, but a `ChangeScoped` decision is reported as
//! `mode: "full"` with the reason [`SCOPED_EXECUTION_NOT_ENABLED`] — see
//! [`ScopeDecision::report`]. Emitting `change-scoped` while running everything
//! would be precisely the lie the contract forbids.

use crate::config::DetectedProfile;
use crate::git::{ChangedPath, FileStatus};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Reported reason when the decision says a narrower run would be sound but
/// this build still executes the full command.
pub const SCOPED_EXECUTION_NOT_ENABLED: &str = "scoped execution not enabled yet";

/// Hard ceiling for the one `cargo metadata` call a run makes.
///
/// Measured at well under a second on every repo the contract falsified
/// against (prview-rs, Vista `src-tauri`, aicx, a lockfile-less crate), so this
/// is a stall guard, not a budget: cargo can sit on a file lock held by a
/// parallel build, and the run must escalate to a full test pass rather than
/// wait on it.
pub const CARGO_METADATA_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// The change set (the seam)
// ---------------------------------------------------------------------------

/// The change a run is reviewing, as the checks see it.
///
/// Carried on [`crate::config::Config`] as internal runtime state, never a CLI
/// or manifest override, and set once on the cloned check config next to
/// `pinned_target` / `pinned_diff_bases`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    paths: Vec<ChangedPath>,
    /// Exactly one base produced this set. More than one base means more than
    /// one review range, and a selection made from the union of several ranges
    /// cannot be pinned to any of them — the same reason Semgrep drops to a
    /// full scan when the run has multiple bases.
    single_base: bool,
}

impl ChangeSet {
    pub fn new(paths: Vec<ChangedPath>, single_base: bool) -> Self {
        Self { paths, single_base }
    }

    pub fn paths(&self) -> &[ChangedPath] {
        &self.paths
    }

    pub fn single_base(&self) -> bool {
        self.single_base
    }
}

/// The tree the checks actually read, and how it relates to the reviewed
/// commit.
///
/// This is the second half of "can a selection be trusted". The change set says
/// what changed between two commits; this says whether the tree the tools read
/// is exactly that. The two are separate facts and only knowable at different
/// times — the substrate is decided by `share_target_snapshot` inside
/// `checks::run_all`, long after the diff exists — so keeping them apart is
/// what stops the flag from being a constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewedTree {
    /// The run materialised a snapshot of the reviewed commit and the checks
    /// read that. The operator's own checkout is irrelevant to the review.
    Snapshot(PathBuf),
    /// No snapshot: the checks read the operator checkout, and it is exactly
    /// the reviewed commit.
    LocalClean(PathBuf),
    /// No snapshot: the checks read the operator checkout, and it carries work
    /// the commit range does not list.
    LocalDirty(PathBuf),
    /// The substrate could not be established.
    Unknown(PathBuf),
}

impl ReviewedTree {
    /// What the run ended up reading.
    ///
    /// `scan_dir` is the shared snapshot the run materialised, as the ledger
    /// records it; `None` means `plan_check_run` returned the repository root
    /// because the reviewed target IS the checked-out `HEAD`. Only in that
    /// second case does the operator's working tree enter the picture at all.
    pub fn resolve(
        repo_root: &Path,
        scan_dir: Option<PathBuf>,
        operator_worktree_clean: Option<bool>,
    ) -> Self {
        match scan_dir {
            Some(snapshot) => Self::Snapshot(snapshot),
            None => match operator_worktree_clean {
                Some(true) => Self::LocalClean(repo_root.to_path_buf()),
                Some(false) => Self::LocalDirty(repo_root.to_path_buf()),
                None => Self::Unknown(repo_root.to_path_buf()),
            },
        }
    }

    /// The directory the checks read. Repo-relative diff paths and
    /// `cargo metadata` manifest paths must both be spoken in ITS coordinates.
    pub fn root(&self) -> &Path {
        match self {
            Self::Snapshot(root)
            | Self::LocalClean(root)
            | Self::LocalDirty(root)
            | Self::Unknown(root) => root,
        }
    }

    /// Whether the tree the checks read contains exactly the reviewed commit
    /// and nothing else.
    ///
    /// Note carefully what this does and does not say about a dirty checkout.
    /// When the run reads a SNAPSHOT, the operator's uncommitted work is not in
    /// the reviewed tree at all — the review is about the pinned target and the
    /// canonical PR diff, so a dirty checkout is emphatically NOT a reason to
    /// widen the run; escalating there would make the feature useless during
    /// exactly the work it exists for. When the run reads the operator checkout
    /// itself, the same uncommitted work IS what the tools compile and run,
    /// while the change set cannot list it — and selecting from a set that is
    /// missing files the tools will read is the silent narrowing this contract
    /// forbids.
    pub fn is_exactly_the_reviewed_commit(&self) -> bool {
        matches!(self, Self::Snapshot(_) | Self::LocalClean(_))
    }
}

// ---------------------------------------------------------------------------
// Ecosystems and decisions
// ---------------------------------------------------------------------------

/// A test runner whose scope is decided independently of the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ecosystem {
    Cargo,
    Vitest,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Vitest => "vitest",
        }
    }

    /// The check whose row publishes this ecosystem's scope, by display name.
    pub fn check_name(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo test",
            Self::Vitest => "Vitest",
        }
    }
}

/// What must run for one ecosystem, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeDecision {
    /// Everything runs. `reason` is the contract's `escalated_by`: it always
    /// names the specific fact that widened the run.
    Full {
        reason: String,
        /// Changed paths the decision counted before escalating. `None` ONLY
        /// when there was never a set to count — it means "never got far
        /// enough", not "zero".
        inputs: Option<usize>,
    },
    /// A narrower run is provably sufficient for this change.
    ChangeScoped {
        /// Changed paths taken into account.
        inputs: usize,
        /// What the decision selects: package names for Cargo, source file
        /// paths for Vitest (Vitest resolves those into test files itself, so
        /// the count of test files is only knowable after the run).
        selected: Vec<String>,
        /// The whole population the selection was drawn from, when knowable
        /// before the run: workspace members for Cargo. `None` for Vitest,
        /// whose test-file universe only the tool can enumerate.
        universe: Option<usize>,
        /// The paths that would be handed to the selector.
        selector_inputs: Vec<String>,
    },
}

impl ScopeDecision {
    fn full(reason: impl Into<String>, inputs: Option<usize>) -> Self {
        Self::Full {
            reason: reason.into(),
            inputs,
        }
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full { .. })
    }

    /// The reason a check must report as `Skipped` because this decision chose
    /// nothing to run, or `None` when something was selected.
    ///
    /// Not applied to any `CheckResult` in this build: the checks still execute
    /// their full commands, and a run that executed the whole suite may not be
    /// relabelled `Skipped`. Step 2 is where this reason reaches a check's
    /// status, at the same moment the commands actually narrow. Defined and
    /// tested here so the semantics land with the decision that produces them.
    pub fn empty_selection_skip_reason(&self) -> Option<&'static str> {
        match self {
            Self::ChangeScoped { selected, .. } if selected.is_empty() => {
                Some(NO_TESTS_RELATED_TO_THE_CHANGE)
            }
            _ => None,
        }
    }

    /// The publishable view of this decision, given that this build still runs
    /// full commands.
    ///
    /// A real escalation keeps its own reason — that fact is true regardless of
    /// whether scoped execution is wired up. A `ChangeScoped` decision is
    /// reported as `full`, because full is what ran; `selected` and `selector`
    /// stay `null` for the same reason, since nothing was selected and no
    /// selector was invoked. `inputs` is what the decision actually counted,
    /// escalation or not — a full run that examined seven changed paths says
    /// seven, because `null` there is reserved for "there was nothing to
    /// count".
    pub fn report(&self) -> ScopeReport {
        match self {
            Self::Full { reason, inputs } => ScopeReport {
                mode: "full",
                reason: reason.clone(),
                inputs: *inputs,
                selected: None,
                universe: None,
                selector: None,
                non_participating: Vec::new(),
            },
            Self::ChangeScoped {
                inputs, universe, ..
            } => ScopeReport {
                mode: "full",
                reason: SCOPED_EXECUTION_NOT_ENABLED.to_string(),
                inputs: Some(*inputs),
                selected: None,
                universe: *universe,
                selector: None,
                non_participating: Vec::new(),
            },
        }
    }
}

/// What a check must report when the scope selected nothing at all.
///
/// Contract §8.1 and §6a.7: an empty selection is an honest `Skipped` with this
/// reason, NEVER a `passed`. A documentation-only change is the ordinary way to
/// reach it — the relevant set is empty after classification, so there is no
/// test the change could have broken, and saying "passed" would be claiming
/// evidence the run never produced.
pub const NO_TESTS_RELATED_TO_THE_CHANGE: &str = "no tests related to the change";

/// The run's decision for every ecosystem it can scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDecisions {
    pub cargo: ScopeDecision,
    pub vitest: ScopeDecision,
    /// Changed paths this run decided are not inputs to test selection, with
    /// the rule that said so. Ecosystem-independent: the classification is a
    /// property of the path, not of the runner.
    pub non_participating: Vec<NonParticipatingPath>,
}

impl ScopeDecisions {
    pub fn get(&self, ecosystem: Ecosystem) -> &ScopeDecision {
        match ecosystem {
            Ecosystem::Cargo => &self.cargo,
            Ecosystem::Vitest => &self.vitest,
        }
    }

    /// The decision published on a given check's row, if that check is the
    /// ecosystem's test runner. Matching is by display name, the same identity
    /// the merge gate uses to pair a policy row with an executed check.
    pub fn for_check(&self, check_name: &str) -> Option<&ScopeDecision> {
        [Ecosystem::Cargo, Ecosystem::Vitest]
            .into_iter()
            .find(|eco| check_name.eq_ignore_ascii_case(eco.check_name()))
            .map(|eco| self.get(eco))
    }

    /// The publishable `scope` object for a check row: the ecosystem's decision
    /// plus the run's non-participating classification, which is what makes the
    /// decision auditable.
    pub fn report_for_check(&self, check_name: &str) -> Option<ScopeReport> {
        self.for_check(check_name).map(|decision| {
            let mut report = decision.report();
            report.non_participating = self.non_participating.clone();
            report
        })
    }

    /// Review caveats owed to the merge gate because a check ran narrower than
    /// its full suite. Advisory only — scope never changes a verdict.
    ///
    /// Empty in this build: nothing reports `change-scoped` while the commands
    /// stay full. The renderer exists here so the caveat cannot be forgotten
    /// when execution is wired up, and so its wording is pinned by a test now.
    pub fn review_caveats(&self) -> Vec<String> {
        [Ecosystem::Cargo, Ecosystem::Vitest]
            .into_iter()
            .filter(|eco| self.get(*eco).report().mode == "change-scoped")
            .map(|eco| {
                format!(
                    "{} ran a change-scoped test selection; tests unrelated to the diff by static \
                     analysis were not executed",
                    eco.check_name()
                )
            })
            .collect()
    }
}

/// The additive `scope` object published on a check row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeReport {
    pub mode: &'static str,
    pub reason: String,
    /// `null` when the run never got far enough to count inputs.
    pub inputs: Option<usize>,
    /// `null` when nothing was selected — including a full run, which selects
    /// by not selecting.
    pub selected: Option<usize>,
    pub universe: Option<usize>,
    /// The selector's actual arguments. `null` when no selector ran.
    pub selector: Option<String>,
    /// Changed paths classified as not inputs to test selection, each with the
    /// rule that said so (contract §6a.6). Additive to the 3.1 shape and
    /// omitted when empty, so a run that neutralised nothing looks exactly as
    /// it did before.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub non_participating: Vec<NonParticipatingPath>,
}

// ---------------------------------------------------------------------------
// Escalation reasons
// ---------------------------------------------------------------------------

/// Every reason a run can widen to a full test pass, spelled once.
///
/// These strings are the contract's `escalated_by` and land verbatim in
/// `RUN.json`, `report.json` and `MERGE_GATE.json`, so they are part of the
/// artifact contract and are pinned by tests.
pub mod reason {
    pub const NO_CHANGE_SET: &str = "no change set pinned for this run";
    pub const MULTIPLE_BASES: &str = "multiple diff bases";
    /// The checks read the operator checkout and it carries uncommitted work.
    /// Not to be confused with "the operator's checkout is dirty": that alone
    /// is never a reason (see [`super::ReviewedTree::is_exactly_the_reviewed_commit`]).
    pub const CHECKS_READ_AN_UNCOMMITTED_TREE: &str =
        "checks read the operator checkout, which carries work outside the reviewed commit";
    pub const UNKNOWN_REVIEWED_TREE: &str = "the tree the checks read could not be identified";
    pub const NO_CARGO_ROOT: &str = "no cargo root detected";
    pub const NO_JS_SOURCE: &str = "no JavaScript or TypeScript source detected";
    pub const REVIEWED_CARGO_ROOT_UNLOCATABLE: &str =
        "the cargo root could not be located inside the reviewed tree";

    /// A changed path that belongs to no recognised source language and to no
    /// class this table knows how to reason about — a JSON asset, a template,
    /// an `.env` file, a data fixture, documentation. Vitest selects by the
    /// static import graph and cargo by package membership, and neither can see
    /// a file read through `fs` or `include_str!`, so there is nothing to
    /// select on and no proof the change is irrelevant. Contract §2: an unknown
    /// file ends in a full run, never a silent narrowing.
    pub fn unsupported_input(path: &str) -> String {
        format!("unsupported input: {path}")
    }

    pub fn manifest(path: &str) -> String {
        format!("manifest or lockfile changed: {path}")
    }

    pub fn test_tooling_config(path: &str) -> String {
        format!("test tooling config changed: {path}")
    }

    pub fn rust_build_config(path: &str) -> String {
        format!("rust build config changed: {path}")
    }

    pub fn proc_macro(package: &str) -> String {
        format!("proc-macro crate changed: {package}")
    }

    pub fn removed_or_renamed(path: &str) -> String {
        format!("path removed or renamed: {path}")
    }

    pub fn shared_tooling(path: &str) -> String {
        format!("shared tooling changed: {path}")
    }

    pub fn resolution_failed(detail: &str) -> String {
        format!("scope resolution failed: {detail}")
    }
}

// ---------------------------------------------------------------------------
// Cargo workspace metadata
// ---------------------------------------------------------------------------

/// The workspace as `cargo metadata --no-deps` reports it.
///
/// One call per run, on the pinned snapshot, `--frozen` so it can neither reach
/// the network nor write a lockfile. `--no-deps` is a different contract from
/// the full resolve `cargo.rs` refuses: it returns the workspace members'
/// manifests without resolving the dependency graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CargoWorkspace {
    /// Member directory (absolute, from `manifest_path`) → package name.
    members: BTreeMap<PathBuf, String>,
    /// Package name → packages that depend on it by path, one hop.
    path_dependents: BTreeMap<String, BTreeSet<String>>,
    /// Packages that build a proc-macro target.
    proc_macros: BTreeSet<String>,
}

/// Why a workspace could not be read. Every variant escalates to a full run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    /// The cargo root could not be placed inside the reviewed tree, so there is
    /// no correct directory to read metadata from.
    UnlocatableRoot,
    Spawn(String),
    ExitStatus(Option<i32>),
    Unparsable(String),
    NoMembers,
    Timeout,
}

impl WorkspaceError {
    fn detail(&self) -> String {
        match self {
            Self::UnlocatableRoot => reason::REVIEWED_CARGO_ROOT_UNLOCATABLE.to_string(),
            Self::Spawn(err) => format!("cargo metadata could not run: {err}"),
            Self::ExitStatus(Some(code)) => format!("cargo metadata exited with code {code}"),
            Self::ExitStatus(None) => "cargo metadata was terminated by a signal".to_string(),
            Self::Unparsable(err) => format!("cargo metadata emitted unparsable JSON: {err}"),
            Self::NoMembers => "cargo metadata reported no workspace members".to_string(),
            Self::Timeout => "cargo metadata timed out".to_string(),
        }
    }
}

impl CargoWorkspace {
    /// Parse a `cargo metadata --format-version 1 --no-deps` document.
    pub fn from_metadata_json(raw: &[u8]) -> Result<Self, WorkspaceError> {
        let value: serde_json::Value = serde_json::from_slice(raw)
            .map_err(|err| WorkspaceError::Unparsable(err.to_string()))?;
        let packages = value
            .get("packages")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| WorkspaceError::Unparsable("no `packages` array".to_string()))?;

        let mut members = BTreeMap::new();
        let mut path_dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut proc_macros = BTreeSet::new();
        // Manifest directory of every member, so a path dependency can be
        // resolved to the member it points at rather than to its own spelling.
        let mut dir_of: BTreeMap<PathBuf, String> = BTreeMap::new();

        for package in packages {
            // A member without a readable `[package] name` cannot be selected
            // with `-p`, so the whole mapping is unusable rather than partially
            // right.
            let Some(name) = package.get("name").and_then(serde_json::Value::as_str) else {
                return Err(WorkspaceError::Unparsable(
                    "a package has no `name`".to_string(),
                ));
            };
            let Some(manifest_path) = package
                .get("manifest_path")
                .and_then(serde_json::Value::as_str)
            else {
                return Err(WorkspaceError::Unparsable(format!(
                    "package `{name}` has no `manifest_path`"
                )));
            };
            let Some(dir) = Path::new(manifest_path).parent().map(Path::to_path_buf) else {
                return Err(WorkspaceError::Unparsable(format!(
                    "package `{name}` has a manifest with no parent directory"
                )));
            };
            members.insert(dir.clone(), name.to_string());
            dir_of.insert(dir, name.to_string());

            if package
                .get("targets")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|targets| {
                    targets.iter().any(|target| {
                        target
                            .get("kind")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|kinds| {
                                kinds.iter().any(|kind| kind.as_str() == Some("proc-macro"))
                            })
                    })
                })
            {
                proc_macros.insert(name.to_string());
            }
        }

        if members.is_empty() {
            return Err(WorkspaceError::NoMembers);
        }

        for package in packages {
            let Some(name) = package.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(dependencies) = package
                .get("dependencies")
                .and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            for dependency in dependencies {
                // Only path dependencies create an edge inside the workspace. A
                // registry dependency is resolved from the index and cannot make
                // one member's change affect another member's tests.
                //
                // Inherited dependencies (`dep = { workspace = true }`) need no
                // special case: falsified on cargo 1.93.1, `--no-deps` resolves
                // the inheritance before emitting, so an inherited path
                // dependency carries `path` exactly like a directly declared
                // one. That class does NOT escalate.
                let Some(path) = dependency.get("path").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                // Resolve through the directory, not the dependency's declared
                // name: `dep = { package = "real", path = "…" }` renames it.
                let Some(target) = dir_of.get(Path::new(path)) else {
                    continue;
                };
                if target == name {
                    continue;
                }
                path_dependents
                    .entry(target.clone())
                    .or_default()
                    .insert(name.to_string());
            }
        }

        Ok(Self {
            members,
            path_dependents,
            proc_macros,
        })
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    pub fn is_proc_macro(&self, package: &str) -> bool {
        self.proc_macros.contains(package)
    }

    /// The member that owns `file`, by longest matching member directory.
    ///
    /// Longest prefix is what makes nested workspaces correct: a file under
    /// `crates/inner/src` belongs to `crates/inner`, not to the outer member
    /// whose directory is also a prefix of it. `None` means the file is inside
    /// no member at all, which the caller must treat as a reason to run
    /// everything — not as "nothing to run".
    pub fn package_for_file(&self, file: &Path) -> Option<&str> {
        self.members
            .iter()
            .filter(|(dir, _)| file.starts_with(dir))
            .max_by_key(|(dir, _)| dir.as_os_str().len())
            .map(|(_, name)| name.as_str())
    }

    /// `package` plus every member that reaches it through path dependencies,
    /// transitively. Cycles terminate because a package is expanded once.
    pub fn reverse_closure(&self, package: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut queue = vec![package.to_string()];
        while let Some(current) = queue.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            if let Some(dependents) = self.path_dependents.get(&current) {
                queue.extend(dependents.iter().cloned());
            }
        }
        seen
    }
}

/// Read the workspace once, from the pinned snapshot.
///
/// `--frozen` guarantees no network and no lockfile write; `--no-deps` keeps it
/// to the members' own manifests. Any failure at all is the caller's signal to
/// run the full workspace.
pub async fn read_cargo_workspace(cargo_root: &Path) -> Result<CargoWorkspace, WorkspaceError> {
    let output = crate::checks::run_command_with_timeout_and_env(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps", "--frozen"],
        cargo_root,
        CARGO_METADATA_TIMEOUT_SECS,
        &[],
    )
    .await
    .map_err(|err| {
        let text = err.to_string();
        if text.contains("timed out") {
            WorkspaceError::Timeout
        } else {
            WorkspaceError::Spawn(text)
        }
    })?;
    if !output.status.success() {
        return Err(WorkspaceError::ExitStatus(output.status.code()));
    }
    CargoWorkspace::from_metadata_json(&output.stdout)
}

// ---------------------------------------------------------------------------
// File classification
// ---------------------------------------------------------------------------

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Manifests and lockfiles: they can change what *any* test compiles or
/// resolves, in either ecosystem, so they escalate both.
fn is_manifest_or_lockfile(path: &str) -> bool {
    matches!(
        basename(path),
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "pnpm-workspace.yaml"
            | "yarn.lock"
            | "pyproject.toml"
            | "poetry.lock"
            | "uv.lock"
    )
}

/// Configuration that changes what Vitest loads or how it type-checks.
///
/// Known gap, stated rather than hidden: a setup file referenced from
/// `setupFiles` under an unconventional name is not recognised here. Changing
/// the config that names it does escalate, and the contract deliberately does
/// not rely on Vitest's own `forceRerunTriggers`, which is documented only for
/// `--changed`.
fn is_vitest_tooling_config(path: &str) -> bool {
    let name = basename(path);
    let stem_matches = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'))
    };
    stem_matches("vitest.config")
        || stem_matches("vite.config")
        || stem_matches("vitest.workspace")
        || stem_matches("vitest.setup")
        || stem_matches("vite.setup")
        || stem_matches("setupTests")
        || stem_matches("test-setup")
        || stem_matches("globalSetup")
        || (name.starts_with("tsconfig") && name.ends_with(".json"))
}

/// Configuration that changes what cargo builds or how it builds it.
fn is_rust_build_config(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let name = basename(&normalized);
    name == "build.rs"
        || name == "rust-toolchain"
        || name == "rust-toolchain.toml"
        || normalized.ends_with(".cargo/config.toml")
        || normalized.ends_with(".cargo/config")
}

/// Directories whose contents are shared by many tests: a change there can
/// alter the behaviour of tests that never import the changed file by any path
/// the import graph shows.
const SHARED_TOOLING_DIRECTORIES: &[&str] = &[
    "tools",
    "fixtures",
    "__fixtures__",
    "__mocks__",
    "test-helpers",
    "test_helpers",
    "testutils",
];

/// Path segments, separator-normalised.
///
/// Matching whole segments rather than substrings is what makes a REPO-ROOT
/// `__fixtures__/user.ts` match: a `contains("/__fixtures__/")` test needs a
/// leading separator the path does not have, so every root-level shared
/// directory silently fell through to "ordinary source" and got scoped instead
/// of escalated.
fn path_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != ".")
}

fn is_shared_tooling(path: &str) -> bool {
    path_segments(path).any(|segment| SHARED_TOOLING_DIRECTORIES.contains(&segment))
}

// ---------------------------------------------------------------------------
// Three-state classification of a changed path (contract §6a)
// ---------------------------------------------------------------------------

/// What a changed path is, as far as TEST SELECTION is concerned.
///
/// The strict reading of contract §2 — an unknown file ends in a full run —
/// made almost every real pull request escalate, because almost every pull
/// request also touches a CHANGELOG, a document or a workflow file. The answer
/// (Monika, 2026-09-15, §6a) is deliberately NOT an ignore list. It is an
/// explicit third state, so that "we know this is not an input" is recorded as
/// a different fact from "we do not know what this is".
///
/// This classification decides ONE thing: whether a path participates in
/// choosing which tests to run. It does not remove the file from the diff, the
/// artifacts, the signals or the verdict, and it never makes a finding
/// disappear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass<'a> {
    /// Recognised source of an ecosystem: participates in selection.
    Relevant(Ecosystem),
    /// A narrow, NAMED class we can say is not an input to test selection.
    /// Skipped for selection, and it does not escalate. Carries the rule that
    /// said so, so the call can be challenged without reading this file.
    NonParticipating(&'a str),
    /// Everything else. Still escalates to a full run.
    Unknown,
}

impl PathClass<'_> {
    /// The rule that named this path non-participating, if that is what it is.
    pub fn non_participating_rule(&self) -> Option<&str> {
        match self {
            Self::NonParticipating(rule) => Some(rule),
            Self::Relevant(_) | Self::Unknown => None,
        }
    }
}

/// One built-in reason a path is not an input to test selection.
///
/// Each rule is named, and the name is published next to the path it matched.
/// The list is deliberately tiny: everything on it is something whose content
/// no test runner loads, in any ecosystem, by any mechanism we know of. A rule
/// that needs a "usually" or a "probably" does not belong here — that is what
/// `Unknown` is for.
///
/// A worked example of the bar, and of why it is set where it is: the
/// repository's root `README` is deliberately NOT here. Rust crates really do
/// pull it into the build with `#![doc = include_str!("../README.md")]`, which
/// puts it in front of `cargo test --doc`, so a built-in rule calling it
/// neutral would be a false neutral — a silently missed test — in every
/// repository that does. A repository whose tests demonstrably never read it
/// can opt it in through `[scope] non_participating`.
struct BuiltinRule {
    name: &'static str,
    matches: fn(&str) -> bool,
}

/// Prose extensions. A `docs/` directory can also hold a JSON schema or a
/// script that a test really does read, so the documentation rules are bounded
/// by extension rather than by directory alone.
const DOCUMENTATION_EXTENSIONS: &[&str] = &[".md", ".mdx", ".rst", ".txt", ".adoc"];

fn is_documentation_file(path: &str) -> bool {
    DOCUMENTATION_EXTENSIONS
        .iter()
        .any(|extension| path.to_ascii_lowercase().ends_with(extension))
}

fn is_at_repository_root(path: &str) -> bool {
    path_segments(path).count() == 1
}

fn root_file_named(path: &str, prefix: &str) -> bool {
    is_at_repository_root(path)
        && basename(path)
            .to_ascii_uppercase()
            .starts_with(&prefix.to_ascii_uppercase())
}

const BUILTIN_NON_PARTICIPATING: &[BuiltinRule] = &[
    // The release log. Its content is never loaded by a test runner, and it
    // changes in almost every pull request — which is exactly why the strict
    // rule made scoping unreachable.
    BuiltinRule {
        name: "root-changelog",
        matches: |path| root_file_named(path, "CHANGELOG"),
    },
    // Licence text at the repository root. Both spellings.
    BuiltinRule {
        name: "root-license",
        matches: |path| root_file_named(path, "LICENSE") || root_file_named(path, "LICENCE"),
    },
    // Prose under a top-level documentation directory. NOT Markdown wholesale:
    // a `.md` anywhere else — a fixture, a snapshot, a test's own input — stays
    // `Unknown` and still escalates.
    BuiltinRule {
        name: "docs-directory",
        matches: |path| {
            matches!(path_segments(path).next(), Some("docs") | Some("doc"))
                && is_documentation_file(path)
        },
    },
    // CI workflow definitions. They describe how CI runs prview; they are not
    // read by the code under test. Scoped to `.github/workflows/` only —
    // composite actions and scripts elsewhere under `.github/` are not covered.
    BuiltinRule {
        name: "ci-workflow",
        matches: |path| {
            let mut segments = path_segments(path);
            segments.next() == Some(".github")
                && segments.next() == Some("workflows")
                && (path.ends_with(".yml") || path.ends_with(".yaml"))
        },
    },
];

/// Decides the three states, built-ins plus whatever the repository declared.
pub struct PathClassifier {
    builtins_enabled: bool,
    /// Repo-declared patterns, kept with their source text so the report can
    /// name the pattern that matched rather than an opaque index.
    repo_patterns: Vec<(String, glob::Pattern)>,
}

impl PathClassifier {
    /// Build from a repository's manifest settings.
    ///
    /// An unparsable pattern is DROPPED rather than applied loosely: a rule we
    /// cannot compile cannot be allowed to neutralise a path by accident, and
    /// the strict side of this decision is the safe side. The dropped pattern
    /// is returned so the caller can say so out loud.
    pub fn new(builtins_enabled: bool, patterns: &[String]) -> (Self, Vec<String>) {
        let mut repo_patterns = Vec::new();
        let mut rejected = Vec::new();
        for pattern in patterns {
            match glob::Pattern::new(pattern) {
                Ok(compiled) => repo_patterns.push((pattern.clone(), compiled)),
                Err(_) => rejected.push(pattern.clone()),
            }
        }
        (
            Self {
                builtins_enabled,
                repo_patterns,
            },
            rejected,
        )
    }

    /// The strictest classifier: nothing is neutral, every unrecognised path
    /// escalates. This is what `non_participating_builtins = false` restores.
    pub fn strict() -> Self {
        Self {
            builtins_enabled: false,
            repo_patterns: Vec::new(),
        }
    }

    /// The rule that says this path is not an input to test selection, if any.
    fn non_participating(&self, path: &str) -> Option<&str> {
        if self.builtins_enabled
            && let Some(rule) = BUILTIN_NON_PARTICIPATING
                .iter()
                .find(|rule| (rule.matches)(path))
        {
            return Some(rule.name);
        }
        self.repo_patterns
            .iter()
            .find(|(_, compiled)| compiled.matches(path))
            .map(|(source, _)| source.as_str())
    }

    /// Full three-state classification.
    ///
    /// Recognised source wins over every neutral rule: a repository cannot
    /// declare its own `src/**/*.ts` neutral and quietly stop testing it.
    pub fn classify<'a>(&'a self, path: &str) -> PathClass<'a> {
        if let Some(ecosystem) = owner(path) {
            return PathClass::Relevant(ecosystem);
        }
        match self.non_participating(path) {
            Some(rule) => PathClass::NonParticipating(rule),
            None => PathClass::Unknown,
        }
    }
}

fn is_rust_source(path: &str) -> bool {
    path.ends_with(".rs")
}

fn is_js_source(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".svelte",
    ];
    EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Which ecosystem owns a path, for the classes whose escalation is scoped to
/// "the ecosystem owning the file". `None` means no single owner — and the
/// caller escalates both rather than guessing.
fn owner(path: &str) -> Option<Ecosystem> {
    if is_rust_source(path) {
        Some(Ecosystem::Cargo)
    } else if is_js_source(path) {
        Some(Ecosystem::Vitest)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// Everything [`decide`] needs that is not the change itself.
pub struct ScopeInputs<'a> {
    /// The tree the checks actually read, and the root its paths live under.
    ///
    /// `cargo metadata` reports absolute manifest paths, while a diff reports
    /// repo-relative ones; they have to be spoken in the same coordinates
    /// before a file can be matched to a member — and those coordinates are the
    /// REVIEWED tree's, not the operator checkout's. On a `--pr` run the two
    /// are different revisions. When they cannot be reconciled the file lands
    /// outside every member, which escalates: wrong here means "run
    /// everything", never "select the wrong package".
    pub reviewed_tree: &'a ReviewedTree,
    pub profile: &'a DetectedProfile,
    /// The workspace read for this run, or the failure that prevented it.
    /// `None` when Rust is not part of the profile at all.
    pub cargo_workspace: Option<&'a Result<CargoWorkspace, WorkspaceError>>,
    /// Paths that are build output rather than source, as the JS checks already
    /// classify them. Injected so the decision stays pure.
    pub is_generated: &'a dyn Fn(&str) -> bool,
    /// The three-state classifier (contract §6a), built from the repository's
    /// manifest settings.
    pub classifier: &'a PathClassifier,
}

/// A changed path the run decided is not an input to test selection, and the
/// rule that said so.
///
/// Published so the call can be challenged without reading the source: a
/// reviewer who disagrees that `docs/architecture.md` is neutral can see the
/// exact rule name and argue with THAT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NonParticipatingPath {
    pub path: String,
    pub rule: String,
}

/// Decide, per ecosystem, what must run for this change.
///
/// Escalation is one-directional: the first reason that widens an ecosystem is
/// the reason reported, and no later file narrows it again.
pub fn decide(change_set: Option<&ChangeSet>, inputs: &ScopeInputs<'_>) -> ScopeDecisions {
    let Some(change_set) = change_set else {
        return both(ScopeDecision::full(reason::NO_CHANGE_SET, None));
    };
    let counted = Some(change_set.paths().len());
    if !change_set.single_base {
        return both(ScopeDecision::full(reason::MULTIPLE_BASES, counted));
    }
    match inputs.reviewed_tree {
        ReviewedTree::Snapshot(_) | ReviewedTree::LocalClean(_) => {}
        ReviewedTree::LocalDirty(_) => {
            return both(ScopeDecision::full(
                reason::CHECKS_READ_AN_UNCOMMITTED_TREE,
                counted,
            ));
        }
        ReviewedTree::Unknown(_) => {
            return both(ScopeDecision::full(reason::UNKNOWN_REVIEWED_TREE, counted));
        }
    }

    let mut cargo_escalation: Option<String> = None;
    let mut vitest_escalation: Option<String> = None;
    let mut escalate = |eco: Option<Ecosystem>, why: String| match eco {
        Some(Ecosystem::Cargo) => {
            cargo_escalation.get_or_insert(why);
        }
        Some(Ecosystem::Vitest) => {
            vitest_escalation.get_or_insert(why);
        }
        None => {
            cargo_escalation.get_or_insert(why.clone());
            vitest_escalation.get_or_insert(why);
        }
    };

    // Profile facts are escalations too: an ecosystem whose root we never found
    // has no basis for a selection.
    let workspace = match inputs.cargo_workspace {
        None => {
            escalate(Some(Ecosystem::Cargo), reason::NO_CARGO_ROOT.to_string());
            None
        }
        Some(Err(err)) => {
            escalate(
                Some(Ecosystem::Cargo),
                reason::resolution_failed(&err.detail()),
            );
            None
        }
        Some(Ok(workspace)) => Some(workspace),
    };
    if !inputs.profile.has_js_source {
        escalate(Some(Ecosystem::Vitest), reason::NO_JS_SOURCE.to_string());
    }

    let mut cargo_packages: BTreeSet<String> = BTreeSet::new();
    let mut cargo_selector_inputs: Vec<String> = Vec::new();
    let mut vitest_inputs: Vec<String> = Vec::new();
    let mut non_participating: Vec<NonParticipatingPath> = Vec::new();

    for change in change_set.paths() {
        let path = change.path.as_str();

        if is_manifest_or_lockfile(path) {
            escalate(None, reason::manifest(path));
            continue;
        }
        if is_rust_build_config(path) {
            escalate(Some(Ecosystem::Cargo), reason::rust_build_config(path));
            continue;
        }
        if is_vitest_tooling_config(path) {
            escalate(Some(Ecosystem::Vitest), reason::test_tooling_config(path));
            continue;
        }
        if is_shared_tooling(path) {
            escalate(owner(path), reason::shared_tooling(path));
            continue;
        }
        // Contract §6a. Checked AFTER every escalating class above, so a
        // document that also lives in a shared tooling directory still
        // escalates: the narrow neutral list never overrides a named reason to
        // widen. A rename only counts as neutral when BOTH ends are — moving
        // `src/a.ts` to `docs/a.md` removes a real source file, and the side the
        // content left still has to escalate.
        if let Some(rule) = inputs.classifier.classify(path).non_participating_rule()
            && change.old_path.as_deref().is_none_or(|old| {
                inputs
                    .classifier
                    .classify(old)
                    .non_participating_rule()
                    .is_some()
            })
        {
            non_participating.push(NonParticipatingPath {
                path: path.to_string(),
                rule: rule.to_string(),
            });
            continue;
        }
        match change.status {
            // A deletion has no surviving file to select from, and a rename
            // moves content out of whatever used to import it. Both escalate
            // the ecosystem that owned the vanished path.
            FileStatus::Deleted => {
                escalate(owner(path), reason::removed_or_renamed(path));
                continue;
            }
            FileStatus::Renamed => {
                let old = change.old_path.as_deref().unwrap_or(path);
                escalate(owner(old), reason::removed_or_renamed(old));
                // The new path is still a legitimate selector input for the
                // ecosystem that now owns it; the escalation above covers the
                // side the content left.
            }
            FileStatus::Added | FileStatus::Modified | FileStatus::Copied => {}
        }

        // Build output is not source: selecting on it would hand the selector
        // paths no test imports.
        if (inputs.is_generated)(path) {
            continue;
        }

        if is_rust_source(path) {
            cargo_selector_inputs.push(path.to_string());
            match workspace {
                Some(workspace) => {
                    match workspace.package_for_file(&inputs.reviewed_tree.root().join(path)) {
                        Some(package) => {
                            if workspace.is_proc_macro(package) {
                                escalate(Some(Ecosystem::Cargo), reason::proc_macro(package));
                            } else {
                                cargo_packages.extend(workspace.reverse_closure(package));
                            }
                        }
                        None => escalate(
                            Some(Ecosystem::Cargo),
                            reason::resolution_failed(&format!(
                                "{path} is outside every workspace member"
                            )),
                        ),
                    }
                }
                None => { /* already escalated */ }
            }
        } else if is_js_source(path) {
            vitest_inputs.push(path.to_string());
        } else {
            // `Unknown`, and it stays escalating (contract §6a.1). Neither
            // selector can see this file: vitest walks the static import graph
            // and cargo walks package membership, while a locale file, a
            // template or a data fixture is read at runtime through `fs` or
            // `include_str!` from a path neither graph contains. There is
            // nothing to select on and no proof the change is irrelevant, so it
            // escalates BOTH ecosystems rather than quietly contributing
            // nothing to either selection. The third state exists only to
            // separate this case from the ones we can actually name.
            escalate(None, reason::unsupported_input(path));
        }
    }

    let inputs_considered = change_set.paths().len();
    let cargo = match cargo_escalation {
        Some(reason) => ScopeDecision::Full {
            reason,
            inputs: Some(inputs_considered),
        },
        None => ScopeDecision::ChangeScoped {
            inputs: inputs_considered,
            selected: cargo_packages.into_iter().collect(),
            universe: workspace.map(CargoWorkspace::member_count),
            selector_inputs: cargo_selector_inputs,
        },
    };
    let vitest = match vitest_escalation {
        Some(reason) => ScopeDecision::Full {
            reason,
            inputs: Some(inputs_considered),
        },
        None => ScopeDecision::ChangeScoped {
            inputs: inputs_considered,
            selected: vitest_inputs.clone(),
            universe: None,
            selector_inputs: vitest_inputs,
        },
    };
    ScopeDecisions {
        cargo,
        vitest,
        non_participating,
    }
}

fn both(decision: ScopeDecision) -> ScopeDecisions {
    ScopeDecisions {
        cargo: decision.clone(),
        vitest: decision,
        non_participating: Vec::new(),
    }
}

/// Resolve the run's scope once: read the workspace if (and only if) a
/// selection could possibly be made, then decide.
///
/// The metadata call is skipped entirely when the change set is missing or
/// untrustworthy, because the answer is already "run everything" and paying for
/// a subprocess to confirm it would be work the contract exists to avoid.
pub async fn resolve_run_scope(
    config: &crate::config::Config,
    reviewed_tree: &ReviewedTree,
) -> ScopeDecisions {
    let change_set = config.changed_paths.as_ref();
    // `cargo metadata` resolves manifest paths through the real directory, so
    // the root the diff's paths are joined onto has to be resolved the same way
    // or nothing will match. A root that cannot be canonicalised is used as
    // given; the worst case is an unmatched file, which escalates.
    let reviewed_tree = canonicalized(reviewed_tree);
    // Built from the repository's own manifest settings: a repo may add its own
    // neutral patterns or turn the built-in list off entirely, and turning it
    // off restores strictly escalating behaviour.
    let (classifier, rejected_patterns) = PathClassifier::new(
        config.scope_non_participating_builtins,
        &config.scope_non_participating,
    );
    for pattern in &rejected_patterns {
        // Loud, and strict: a rule we cannot compile is dropped rather than
        // applied loosely, so an unparsable pattern can never neutralise a path
        // by accident.
        eprintln!(
            "warning: ignoring unparsable `[scope] non_participating` pattern `{pattern}`; \
             paths it was meant to cover will escalate to a full test run"
        );
    }
    let can_select = change_set.is_some_and(ChangeSet::single_base)
        && reviewed_tree.is_exactly_the_reviewed_commit();
    if !can_select {
        // No subprocess: the answer is already "run everything", and paying for
        // a `cargo metadata` to confirm it would be exactly the work this
        // contract exists to avoid.
        return decide(
            change_set,
            &ScopeInputs {
                reviewed_tree: &reviewed_tree,
                profile: &config.profile,
                cargo_workspace: None,
                is_generated: &|_| false,
                classifier: &classifier,
            },
        );
    }
    let workspace = match reviewed_cargo_root(config, reviewed_tree.root()) {
        CargoRoot::None => None,
        CargoRoot::Reviewed(root) => Some(read_cargo_workspace(&root).await),
        CargoRoot::Unlocatable => Some(Err(WorkspaceError::UnlocatableRoot)),
    };
    decide(
        change_set,
        &ScopeInputs {
            reviewed_tree: &reviewed_tree,
            profile: &config.profile,
            cargo_workspace: workspace.as_ref(),
            is_generated: &|path| crate::checks::is_generated_artifact_path(path, config),
            classifier: &classifier,
        },
    )
}

fn canonicalized(tree: &ReviewedTree) -> ReviewedTree {
    let resolved = tree
        .root()
        .canonicalize()
        .unwrap_or_else(|_| tree.root().to_path_buf());
    match tree {
        ReviewedTree::Snapshot(_) => ReviewedTree::Snapshot(resolved),
        ReviewedTree::LocalClean(_) => ReviewedTree::LocalClean(resolved),
        ReviewedTree::LocalDirty(_) => ReviewedTree::LocalDirty(resolved),
        ReviewedTree::Unknown(_) => ReviewedTree::Unknown(resolved),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CargoRoot {
    /// Rust is not part of this repository at all.
    None,
    /// Where the cargo root lives INSIDE the reviewed tree.
    Reviewed(PathBuf),
    /// Rust is present, but its root cannot be placed inside the reviewed tree.
    Unlocatable,
}

/// Where to read `cargo metadata` from.
///
/// `profile.cargo_root` is detected in the OPERATOR's checkout, which on a
/// `--pr` or `--remote` run is a different revision from the one under review.
/// Reading metadata there would describe another revision's members and path
/// edges while the change set describes this one — members could be missing,
/// added, or moved between them, and the resulting selection would be drawn
/// from the wrong workspace. So the detected root is re-expressed relative to
/// the repository root and rebased onto the reviewed tree.
///
/// When that cannot be done — a cargo root outside the repository, or roots
/// that will not canonicalise onto each other — the answer is not "guess":
/// Rust escalates to a full workspace run with that stated reason.
fn reviewed_cargo_root(config: &crate::config::Config, reviewed_root: &Path) -> CargoRoot {
    let Some(detected) = &config.profile.cargo_root else {
        return CargoRoot::None;
    };
    let repo_root = config
        .repo_root
        .canonicalize()
        .unwrap_or_else(|_| config.repo_root.clone());
    let detected = detected.canonicalize().unwrap_or_else(|_| detected.clone());
    rebase_cargo_root(&detected, &repo_root, reviewed_root)
}

/// The pure half of [`reviewed_cargo_root`]: express the detected root relative
/// to the repository and re-root it on the reviewed tree. A root that is not
/// inside the repository has no reviewed counterpart to compute.
fn rebase_cargo_root(detected: &Path, repo_root: &Path, reviewed_root: &Path) -> CargoRoot {
    match detected.strip_prefix(repo_root) {
        Ok(relative) => CargoRoot::Reviewed(reviewed_root.join(relative)),
        Err(_) => CargoRoot::Unlocatable,
    }
}

#[cfg(test)]
mod tests;
