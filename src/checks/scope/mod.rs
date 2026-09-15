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
    /// The paths were captured from the same pinned target commit the checks
    /// will read. When they were not, the gates would scan a tree containing
    /// work this set never listed, and selecting from it would silently drop
    /// exactly that work.
    snapshot_consistent: bool,
}

impl ChangeSet {
    pub fn new(paths: Vec<ChangedPath>, single_base: bool, snapshot_consistent: bool) -> Self {
        Self {
            paths,
            single_base,
            snapshot_consistent,
        }
    }

    pub fn paths(&self) -> &[ChangedPath] {
        &self.paths
    }

    /// Whether a selection may be made from this set at all.
    ///
    /// Note what is deliberately absent: the operator's working tree. A dirty
    /// checkout is not a reason to widen the run. A `--pr` review is about the
    /// pinned target and the canonical PR diff, so uncommitted files in the
    /// operator's checkout have nothing to do with it — escalating on them
    /// would make the feature useless during ordinary work. What escalates is
    /// an untrustworthy *set*, which is a different fact.
    pub fn is_trustworthy(&self) -> bool {
        self.single_base && self.snapshot_consistent
    }

    pub fn single_base(&self) -> bool {
        self.single_base
    }

    pub fn snapshot_consistent(&self) -> bool {
        self.snapshot_consistent
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
    Full { reason: String },
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
    fn full(reason: impl Into<String>) -> Self {
        Self::Full {
            reason: reason.into(),
        }
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full { .. })
    }

    /// The publishable view of this decision, given that this build still runs
    /// full commands.
    ///
    /// A real escalation keeps its own reason — that fact is true regardless of
    /// whether scoped execution is wired up. A `ChangeScoped` decision is
    /// reported as `full`, because full is what ran; `selected` and `selector`
    /// stay `null` for the same reason, since nothing was selected and no
    /// selector was invoked. `inputs` and `universe` are what the decision
    /// actually examined and so are reported as measured.
    pub fn report(&self) -> ScopeReport {
        match self {
            Self::Full { reason } => ScopeReport {
                mode: "full",
                reason: reason.clone(),
                inputs: None,
                selected: None,
                universe: None,
                selector: None,
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
            },
        }
    }
}

/// The run's decision for every ecosystem it can scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDecisions {
    pub cargo: ScopeDecision,
    pub vitest: ScopeDecision,
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
    pub const NOT_SNAPSHOT_CONSISTENT: &str =
        "change set is not consistent with the reviewed snapshot";
    pub const NO_CARGO_ROOT: &str = "no cargo root detected";
    pub const NO_JS_SOURCE: &str = "no JavaScript or TypeScript source detected";

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
    Spawn(String),
    ExitStatus(Option<i32>),
    Unparsable(String),
    NoMembers,
    Timeout,
}

impl WorkspaceError {
    fn detail(&self) -> String {
        match self {
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

/// Helper and fixture surfaces shared by many tests: a change there can alter
/// the behaviour of tests that never import the changed file by a path the
/// import graph shows.
fn is_shared_tooling(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.starts_with("tools/")
        || normalized.contains("/fixtures/")
        || normalized.starts_with("fixtures/")
        || normalized.contains("/__fixtures__/")
        || normalized.contains("/__mocks__/")
        || normalized.contains("/test-helpers/")
        || normalized.contains("/test_helpers/")
        || normalized.contains("/testutils/")
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
    /// Root the change set's repo-relative paths are resolved against.
    ///
    /// `cargo metadata` reports absolute manifest paths, while a diff reports
    /// repo-relative ones; they have to be spoken in the same coordinates
    /// before a file can be matched to a member. When they cannot be — a
    /// symlinked or otherwise non-matching root — the file lands outside every
    /// member, which escalates. Wrong here means "run everything", never
    /// "select the wrong package".
    pub repo_root: &'a Path,
    pub profile: &'a DetectedProfile,
    /// The workspace read for this run, or the failure that prevented it.
    /// `None` when Rust is not part of the profile at all.
    pub cargo_workspace: Option<&'a Result<CargoWorkspace, WorkspaceError>>,
    /// Paths that are build output rather than source, as the JS checks already
    /// classify them. Injected so the decision stays pure.
    pub is_generated: &'a dyn Fn(&str) -> bool,
}

/// Decide, per ecosystem, what must run for this change.
///
/// Escalation is one-directional: the first reason that widens an ecosystem is
/// the reason reported, and no later file narrows it again.
pub fn decide(change_set: Option<&ChangeSet>, inputs: &ScopeInputs<'_>) -> ScopeDecisions {
    let Some(change_set) = change_set else {
        return both(ScopeDecision::full(reason::NO_CHANGE_SET));
    };
    if !change_set.single_base {
        return both(ScopeDecision::full(reason::MULTIPLE_BASES));
    }
    if !change_set.snapshot_consistent {
        return both(ScopeDecision::full(reason::NOT_SNAPSHOT_CONSISTENT));
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
                Some(workspace) => match workspace.package_for_file(&inputs.repo_root.join(path)) {
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
                },
                None => { /* already escalated */ }
            }
        } else if is_js_source(path) {
            vitest_inputs.push(path.to_string());
        }
    }

    let inputs_considered = change_set.paths().len();
    let cargo = match cargo_escalation {
        Some(reason) => ScopeDecision::Full { reason },
        None => ScopeDecision::ChangeScoped {
            inputs: inputs_considered,
            selected: cargo_packages.into_iter().collect(),
            universe: workspace.map(CargoWorkspace::member_count),
            selector_inputs: cargo_selector_inputs,
        },
    };
    let vitest = match vitest_escalation {
        Some(reason) => ScopeDecision::Full { reason },
        None => ScopeDecision::ChangeScoped {
            inputs: inputs_considered,
            selected: vitest_inputs.clone(),
            universe: None,
            selector_inputs: vitest_inputs,
        },
    };
    ScopeDecisions { cargo, vitest }
}

fn both(decision: ScopeDecision) -> ScopeDecisions {
    ScopeDecisions {
        cargo: decision.clone(),
        vitest: decision,
    }
}

/// Resolve the run's scope once: read the workspace if (and only if) a
/// selection could possibly be made, then decide.
///
/// The metadata call is skipped entirely when the change set is missing or
/// untrustworthy, because the answer is already "run everything" and paying for
/// a subprocess to confirm it would be work the contract exists to avoid.
pub async fn resolve_run_scope(config: &crate::config::Config) -> ScopeDecisions {
    let change_set = config.changed_paths.as_ref();
    // `cargo metadata` resolves manifest paths through the real directory, so
    // the root the diff's paths are joined onto has to be resolved the same way
    // or nothing will match. A root that cannot be canonicalised is used as
    // given; the worst case is an unmatched file, which escalates.
    let repo_root = config
        .repo_root
        .canonicalize()
        .unwrap_or_else(|_| config.repo_root.clone());
    if !change_set.is_some_and(ChangeSet::is_trustworthy) {
        return decide(
            change_set,
            &ScopeInputs {
                repo_root: &repo_root,
                profile: &config.profile,
                cargo_workspace: None,
                is_generated: &|_| false,
            },
        );
    }
    let workspace = match &config.profile.cargo_root {
        Some(root) => Some(read_cargo_workspace(root).await),
        None => None,
    };
    decide(
        change_set,
        &ScopeInputs {
            repo_root: &repo_root,
            profile: &config.profile,
            cargo_workspace: workspace.as_ref(),
            is_generated: &|path| crate::checks::is_generated_artifact_path(path, config),
        },
    )
}

#[cfg(test)]
mod tests;
