//! Quality checks system
//!
//! Trait-based check system for running various quality tools.

use crate::cache::Cache;
use crate::config::{Config, ProfileKind};
use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Output;
use std::str::FromStr;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Default timeout for checks (5 minutes — large Rust workspaces need this)
pub const CHECK_TIMEOUT_SECS: u64 = 300;

/// Timeout for test commands (15 minutes - ML projects with model loading need this)
pub const TEST_TIMEOUT_SECS: u64 = 900;

mod cargo;
mod python;
mod semgrep;
mod typescript;

pub(crate) use cargo::validated_cargo_audit_vulnerability_list;
pub use cargo::{
    CargoAuditCheck, CargoCheck, CargoGeigerCheck, CargoTestCheck, ClippyCheck, RustfmtCheck,
};
pub use python::{MypyCheck, PytestCheck, RuffCheck};
pub use semgrep::SemgrepCheck;
pub(crate) use semgrep::output_reports_scan_errors as semgrep_output_reports_scan_errors;
pub(crate) use semgrep::scan_error_paths as semgrep_scan_error_paths;
pub use typescript::{ESLintCheck, StylelintCheck, TypeScriptCheck, VitestCheck};

/// Which tree a check's command actually read.
///
/// Provenance without this is not auditable: `cwd` alone cannot tell a reviewer
/// whether the bytes a gate scanned were the reviewed commit or whatever the
/// operator happened to have uncommitted on disk.
// `Hash` is derived so `TreeState` can be half of a task-ledger substrate key
// (`crate::ledger::SubstrateKey`) — one enum for "which tree", not two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TreeState {
    /// A tree materialised from THIS repository's objects at the reviewed
    /// target commit, unmodified — the scanned bytes are exactly `target_sha`.
    /// Either the ephemeral `git worktree` the language checks share, or the
    /// `git archive` extraction the Loctree heuristics scan; both carry the
    /// commit's tree and nothing from the working directory.
    Snapshot,
    /// Same worktree, but it carries changes the run itself produced (a
    /// generated `Cargo.lock`, a tool writing into the checkout) — the scanned
    /// bytes are NOT exactly `target_sha`.
    SnapshotDirty,
    /// The reviewed commit's tree, unmodified, but with its DEPENDENCIES
    /// borrowed: prview links the operator's `node_modules`/`.venv` into the
    /// snapshot ([`SNAPSHOT_SCAFFOLDING`]) instead of installing what the
    /// target's lockfile pins. The reviewed SOURCE is exactly `target_sha`; the
    /// compiler, plugins, type definitions and runtime the tools loaded came
    /// from the local checkout. A dependency-changing PR is precisely where the
    /// two differ, so this must not be reported as an exact snapshot scan.
    SnapshotBorrowedDeps,
    /// The repo's own working tree with no uncommitted changes — the scanned
    /// bytes are exactly `target_sha`.
    LocalClean,
    /// The repo's own working tree carrying uncommitted changes — the scanned
    /// bytes are NOT exactly `target_sha`.
    LocalDirty,
    /// A directory that is neither this repository's working tree nor one of its
    /// worktrees (e.g. a `cargo_root` configured outside the repo). Whatever
    /// `target_sha` names, it belongs to a DIFFERENT checkout: the scanned bytes
    /// say nothing about the reviewed commit.
    Foreign,
}

impl TreeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::SnapshotDirty => "snapshot-dirty",
            Self::SnapshotBorrowedDeps => "snapshot-borrowed-deps",
            Self::LocalClean => "local-clean",
            Self::LocalDirty => "local-dirty",
            Self::Foreign => "foreign",
        }
    }
}

/// The substrate a check ran against: the commit whose tree was scanned, and
/// whether that tree was a snapshot or the live local working tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSubstrate {
    pub target_sha: Option<String>,
    pub tree_state: Option<TreeState>,
}

/// Resolve the substrate `cwd` sits on — the single source of truth every check
/// records through its provenance.
///
/// `target_sha` is the commit checked out in `cwd` (the snapshot's detached
/// commit, or the repo's `HEAD` for an in-place scan). `tree_state` classifies
/// `cwd` against `repo_root` along two axes — WHICH tree it is, and whether that
/// tree still matches its commit:
///
/// Identity is settled BEFORE position, because position cannot answer it:
///
/// - not this repository at all: `foreign` — a different checkout whose bytes
///   say nothing about the reviewed commit. Being lexically inside `repo_root`
///   proves nothing, since a vendored checkout, a submodule or an in-repo
///   symlink to another clone all live there and `discover` resolves to THEM;
///   their `HEAD` belongs to another project, so calling that the reviewed
///   repository's local tree misreports both fields at once;
/// - this repository, inside `repo_root`: the live working tree, `local-clean` /
///   `local-dirty`;
/// - this repository, outside it — a linked worktree: a target snapshot,
///   `snapshot` / `snapshot-dirty`. A snapshot is not immutable: a check can
///   write into it (a generated `Cargo.lock`), after which its bytes are no
///   longer exactly `target_sha`.
///
/// Dirtiness is `git status --porcelain` (untracked files count, ignored ones do
/// not). In a snapshot the dependency symlinks prview itself creates
/// ([`SNAPSHOT_SCAFFOLDING`]) are excluded: they are the tool's own scaffolding,
/// not a modification of the reviewed tree. They are not free of consequence
/// either — a snapshot is `snapshot-borrowed-deps` when it carries a link THIS
/// command could actually consume, named by `consumable` (see
/// [`consumable_scaffolding`]).
///
/// Best effort: a `cwd` that is not in a git repository yields `None` for both
/// fields rather than a guess, and a status that cannot be read yields a `None`
/// `tree_state` — an unknown substrate stays visibly unknown instead of being
/// certified clean.
pub fn resolve_scan_substrate(cwd: &Path, repo_root: &Path, consumable: &[&str]) -> ScanSubstrate {
    let Ok(repo) = git2::Repository::discover(cwd) else {
        return ScanSubstrate::default();
    };

    let target_sha = repo
        .head()
        .and_then(|head| head.peel_to_commit())
        .map(|commit| commit.id().to_string())
        .ok();

    let is_external =
        crate::paths::normalize_to_repo_relative(&cwd.display().to_string(), repo_root).is_external;

    let tree_state = if !belongs_to_repo(&repo, repo_root) {
        Some(TreeState::Foreign)
    } else if !is_external {
        working_tree_is_dirty(&repo, &[]).map(|dirty| {
            if dirty {
                TreeState::LocalDirty
            } else {
                TreeState::LocalClean
            }
        })
    } else {
        working_tree_is_dirty(&repo, SNAPSHOT_SCAFFOLDING).map(|dirty| match dirty {
            true => TreeState::SnapshotDirty,
            false if borrows_local_dependencies(&repo, consumable) => {
                TreeState::SnapshotBorrowedDeps
            }
            false => TreeState::Snapshot,
        })
    };

    ScanSubstrate {
        target_sha,
        tree_state,
    }
}

/// Paths prview itself materialises inside a target snapshot (see
/// `create_worktree_snapshot`): symlinks to the operator's dependency caches, so
/// a review does not reinstall them. They are never part of the reviewed commit
/// and must not make the snapshot look modified.
const SNAPSHOT_SCAFFOLDING: &[&str] = &["node_modules", ".venv"];

/// Which scaffolding links a given check could actually READ.
///
/// Presence of a link is not consumption of it, and the two must not be
/// confused: a mixed repository has `node_modules` linked into every snapshot,
/// but a cargo or semgrep run resolves nothing through it. Labelling those runs
/// `snapshot-borrowed-deps` states a mixed substrate that never existed — the
/// same class of false claim, pointing the other way, as certifying an exact
/// scan.
///
/// The JS checks resolve their compiler, plugins, type definitions and runtime
/// through `node_modules` (`local_js_bin` looks in `node_modules/.bin` first),
/// so for them a linked tree is genuinely the operator's.
///
/// The Python checks return NOTHING, deliberately. The snapshot still links
/// `.venv` — `create_worktree_snapshot` does not know who will run there — but
/// `plan_python_run` points `UV_PROJECT_ENVIRONMENT` at a prview-owned
/// per-commit directory, so an off-HEAD Python command installs and reads the
/// reviewed dependency set, never the link. Without uv the commands come off
/// `PATH` and use the ambient interpreter, which is not the link either. Should
/// that redirect ever be removed, `.venv` belongs back in this list.
fn consumable_scaffolding(check: &str) -> &'static [&'static str] {
    match check {
        "TypeScript" | "ESLint" | "Vitest" | "Stylelint" => &["node_modules"],
        _ => &[],
    }
}

/// Whether the snapshot actually CARRIES a link this command could consume.
///
/// Presence, not policy: an off-HEAD review of a repo with no local
/// `node_modules` installs nothing and links nothing, and stays an exact
/// snapshot scan. Only a link that exists AND is consumable could have been
/// followed.
///
/// Checked at the WORKTREE ROOT, not at the check's `cwd` — a cargo member runs
/// in a subdirectory while the scaffolding sits at the top of the snapshot.
fn borrows_local_dependencies(repo: &git2::Repository, consumable: &[&str]) -> bool {
    let Some(root) = repo.workdir() else {
        return false;
    };
    consumable.iter().any(|name| root.join(name).is_symlink())
}

/// True when `repo` is the repository rooted at `repo_root` — its own working
/// tree or one of its linked worktrees, which share a common git directory.
///
/// A directory merely being outside `repo_root` proves nothing: an external
/// `cargo_root` discovers a DIFFERENT repository, and labelling that a snapshot
/// of the reviewed commit is exactly the false-provenance this record exists to
/// prevent.
fn belongs_to_repo(repo: &git2::Repository, repo_root: &Path) -> bool {
    let Ok(main) = git2::Repository::discover(repo_root) else {
        return false;
    };
    canonical(repo.commondir()) == canonical(main.commondir())
}

fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `git status --porcelain` is non-empty, ignoring `exempt` top-level paths
/// (untracked files count, ignored ones do not).
///
/// `None` when the status cannot be read — an index lock, a permissions error or
/// a malformed repository. That is an UNKNOWN tree state, not a clean one:
/// reporting "clean" there would let provenance certify that the scanned bytes
/// match the commit precisely when nothing could be verified.
fn working_tree_is_dirty(repo: &git2::Repository, exempt: &[&str]) -> Option<bool> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .include_unmodified(false);
    let statuses = repo.statuses(Some(&mut opts)).ok()?;
    Some(statuses.iter().any(|entry| {
        let path = entry.path().unwrap_or_default();
        !exempt
            .iter()
            .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
    }))
}

/// Provenance data for a check execution (Artifact Pack v1)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckProvenance {
    pub command: String,
    pub tool_version: Option<String>,
    pub cwd: String,
    /// Commit whose tree the check scanned. Additive and optional: absent from
    /// artifacts written before this field existed, and when the substrate is
    /// not a git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_sha: Option<String>,
    /// Whether `cwd` was a target snapshot or the live local tree. Additive and
    /// optional — see `target_sha`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_state: Option<TreeState>,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub finished_at: String,
    pub hard_fail_signatures: Vec<String>,
    pub cache_key: Option<String>,
}

impl CheckProvenance {
    /// Record the substrate the check ran on, resolved from the directory the
    /// command actually ran in. Checks that build their provenance literally
    /// (rather than through [`ProvenanceBuilder`]) chain this so the resolution
    /// logic stays in one place.
    ///
    /// `check` is the check's own [`Check::name`]: which dependency links the
    /// classification may hold against this run is a property of the command,
    /// not of the directory (see [`consumable_scaffolding`]).
    #[must_use]
    pub fn with_scan_substrate(mut self, check: &str, cwd: &Path, repo_root: &Path) -> Self {
        let substrate = resolve_scan_substrate(cwd, repo_root, consumable_scaffolding(check));
        self.target_sha = substrate.target_sha;
        self.tree_state = substrate.tree_state;
        self
    }
}

/// A check that was configured but could not run
#[derive(Debug, Clone, Serialize)]
pub struct SkippedCheck {
    pub id: String,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckEligibility {
    Run,
    Skip(String),
}

/// Result of a check execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub duration: Duration,
    pub output: String,
    pub cached: bool,
    /// Provenance for Artifact Pack v1 (None for cached/legacy results)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<CheckProvenance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Passed,
    Failed,
    Warnings,
    Skipped,
    Error,
}

impl FromStr for CheckStatus {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "passed" => Self::Passed,
            "failed" => Self::Failed,
            "warnings" => Self::Warnings,
            "skipped" => Self::Skipped,
            _ => Self::Error,
        })
    }
}

impl CheckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Warnings => "warnings",
            Self::Skipped => "skipped",
            Self::Error => "error",
        }
    }

    /// Every spelling a check status can be written as in an artifact.
    ///
    /// `as_str` is total and this is its image, so a reader can tell "a status I
    /// do not recognize" from "a status that is not a warning" — the difference
    /// between counting an unreadable pack as clean and reporting it. Kept in
    /// step with `as_str` by `every_emitted_status_is_in_the_vocabulary`.
    pub const EMITTED: [&'static str; 5] = ["passed", "failed", "warnings", "skipped", "error"];
}

/// Trait for implementing checks
#[async_trait]
pub trait Check: Send + Sync {
    /// Human-readable name
    fn name(&self) -> &str;

    /// Check if this check can run in current context
    fn check_eligibility(&self, config: &Config) -> CheckEligibility;

    /// Run the check
    async fn run(&self, config: &Config) -> Result<CheckResult>;

    /// Get cache key (None = not cacheable)
    fn cache_key(&self, _config: &Config) -> Option<String> {
        None
    }
}

/// The substrate a ledger entry is keyed on.
///
/// A check that ran resolved its own substrate and reported it in its
/// provenance — that is the tree it actually read, and no second resolution can
/// be more authoritative. A check with no provenance (an eligibility skip; a
/// cache entry written before provenance was stored) falls back to the
/// substrate the run resolved, and to "unknown" when even that is unset.
fn ledger_substrate(provenance: Option<&CheckProvenance>, ledger: &TaskLedger) -> SubstrateKey {
    match provenance {
        Some(prov) => SubstrateKey {
            target_sha: prov.target_sha.clone(),
            tree_state: prov.tree_state,
        },
        None => ledger.resolved_substrate().unwrap_or_default(),
    }
}

/// Run all applicable checks with caching (parallel execution, streaming output).
pub async fn run_all(
    config: &Config,
    ledger: &TaskLedger,
) -> Result<(Vec<CheckResult>, Vec<SkippedCheck>)> {
    run_all_checks(
        get_checks_for_profile(config),
        Cache::new(config),
        config,
        ledger,
    )
    .await
}

/// [`run_all`] with the check set and cache handed in, so a test can drive the
/// runner without a profile detection and a `PRVIEW_HOME`-rooted cache.
async fn run_all_checks(
    checks: Vec<Box<dyn Check>>,
    cache: Cache,
    config: &Config,
    ledger: &TaskLedger,
) -> Result<(Vec<CheckResult>, Vec<SkippedCheck>)> {
    use colored::Colorize;
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::io::Write;
    use std::sync::Arc;

    let cache = Arc::new(cache);
    let emit = !config.json && !config.quiet;

    if emit {
        println!("{}", "Running quality checks...".cyan());
        println!();
    }

    // Separate checks into cached, skipped, and runnable
    let mut results = Vec::new();
    let mut skipped = Vec::new();
    let mut runnable_checks = Vec::new();

    for check in checks {
        match check.check_eligibility(config) {
            CheckEligibility::Skip(reason) => {
                ledger.record(TaskEntry {
                    key: TaskKey::new(check.name(), ledger_substrate(None, ledger)),
                    kind: TaskKind::Check,
                    state: TaskState::Skipped {
                        reason: reason.clone(),
                    },
                    queued_at: None,
                    started_at: None,
                });
                skipped.push(build_skipped_check(check.as_ref(), reason));
                continue;
            }
            CheckEligibility::Run => {}
        }

        if let Some(result) = load_cached_result(check.as_ref(), config, cache.as_ref()) {
            let origin = ledger_substrate(result.provenance.as_ref(), ledger);
            ledger.record(TaskEntry {
                key: TaskKey::new(check.name(), origin.clone()),
                kind: TaskKind::Check,
                state: TaskState::Cached {
                    cache_age_secs: None,
                    origin,
                },
                queued_at: None,
                started_at: None,
            });
            let status_str = format_status(result.status);
            if emit {
                println!("  {} {} (cached)", status_str, check.name());
            }
            results.push(result);
            continue;
        }

        runnable_checks.push(check);
    }

    // Pre-sync Python venv if any Python checks will run and uv is available.
    // This separates venv build time from the per-check timeout budget.
    let has_python_checks = runnable_checks
        .iter()
        .any(|c| matches!(c.name(), "Ruff" | "Mypy" | "Pytest"));
    if has_python_checks && config.profile.runs_python_checks() && which::which("uv").is_ok() {
        if emit {
            print!("  {} Syncing Python venv...", "●".blue());
            let _ = std::io::stdout().flush();
        }
        match run_command_with_timeout(
            "uv",
            &["sync", "--quiet"],
            &config.repo_root,
            CHECK_TIMEOUT_SECS,
        )
        .await
        {
            Ok(output) => {
                if emit {
                    if output.status.success() {
                        print!("\r\x1b[2K  {} Python venv ready\n", "✓".green());
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        print!(
                            "\r\x1b[2K  {} uv sync failed: {}\n",
                            "⚠".yellow(),
                            stderr.lines().next().unwrap_or("unknown error")
                        );
                    }
                    let _ = std::io::stdout().flush();
                }
            }
            Err(e) => {
                if emit {
                    print!("\r\x1b[2K  {} uv sync: {}\n", "⚠".yellow(), e);
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    if !runnable_checks.is_empty() {
        let mut remaining: Vec<String> = runnable_checks
            .iter()
            .map(|c| c.name().to_string())
            .collect();

        if emit {
            print!("  {} Running: {}", "●".blue(), remaining.join(", "));
            let _ = std::io::stdout().flush();
        }

        // Materialise ONE shared target snapshot for the whole run so every
        // snapshot-backed check reuses it instead of creating (and cleaning up)
        // its own worktree (thread 1). On failure, leave the override unset so
        // each check falls back to resolving its own plan — the original
        // per-check behaviour. `_shared_snapshot` stays alive until every check
        // has finished.
        let mut config = config.clone();
        let _shared_snapshot = share_target_snapshot(&mut config, &runnable_checks);

        // Launch all checks in parallel, stream results as they complete.
        // Cargo checks share one target/ build lock, so they serialize on a
        // single-permit semaphore while non-cargo checks stay parallel (PV-17).
        let config = Arc::new(config);
        let cargo_lock = Arc::new(Semaphore::new(1));
        let mut futs: FuturesUnordered<_> = runnable_checks
            .into_iter()
            .map(|check| {
                let config = Arc::clone(&config);
                let cache = Arc::clone(&cache);
                let cargo_lock = Arc::clone(&cargo_lock);
                let queued_at = std::time::Instant::now();
                async move {
                    let started_at = std::time::Instant::now();
                    let _permit = if is_cargo_target_check(check.name()) {
                        Some(
                            cargo_lock
                                .acquire()
                                .await
                                .expect("cargo-target semaphore never closed"),
                        )
                    } else {
                        None
                    };
                    let result = execute_live_check(check, config.as_ref(), cache.as_ref()).await;
                    record_executed_check(&result, ledger, queued_at, started_at);
                    result
                }
            })
            .collect();

        // Elapsed timer — ticks every second on the "Running" line
        let start = std::time::Instant::now();
        let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(1));
        timer.tick().await; // consume immediate first tick

        // PV-18: soft thresholds at which we print a one-time "still running"
        // notice, so a slow run informs the user instead of hanging silently.
        // We never abort the run here — each check self-terminates at its own
        // timeout (PV-16); this only keeps the operator informed.
        const SLOW_NOTICE_THRESHOLDS_SECS: [u64; 3] = [60, 300, 900];
        let mut next_slow_notice = 0usize;

        loop {
            tokio::select! {
                biased;

                Some(result) = futs.next() => {
                    // Remove completed check from remaining list
                    remaining.retain(|n| n != &result.name);

                    if emit {
                        // Clear the "Running" line and print the result
                        print!("\r\x1b[2K");
                        let status_str = format_status(result.status);
                        println!(
                            "  {} {} ({:.1}s)",
                            status_str,
                            result.name,
                            result.duration.as_secs_f32(),
                        );

                        // Show updated "Running" line if checks remain
                        if !remaining.is_empty() {
                            let elapsed = start.elapsed().as_secs();
                            print!(
                                "  {} Running: {} ({}s)",
                                "●".blue(),
                                remaining.join(", "),
                                elapsed
                            );
                            let _ = std::io::stdout().flush();
                        }
                    }

                    results.push(result);

                    if remaining.is_empty() {
                        break;
                    }
                }

                _ = timer.tick(), if emit && !remaining.is_empty() => {
                    let elapsed = start.elapsed().as_secs();
                    // PV-18: when a run crosses a soft threshold, print a
                    // one-time note naming what's still running and how to bail.
                    // We inform, we do not abort — the checks own their timeouts.
                    if next_slow_notice < SLOW_NOTICE_THRESHOLDS_SECS.len()
                        && elapsed >= SLOW_NOTICE_THRESHOLDS_SECS[next_slow_notice]
                    {
                        next_slow_notice += 1;
                        println!(
                            "\r\x1b[2K  {} Still running after {}s: {}. Cargo checks compile \
                             the whole workspace and can take several minutes (each has its \
                             own timeout). Press Ctrl-C to abort.",
                            "ℹ".cyan(),
                            elapsed,
                            remaining.join(", "),
                        );
                    }
                    // Update elapsed time on the "Running" line
                    print!(
                        "\r\x1b[2K  {} Running: {} ({}s)",
                        "●".blue(),
                        remaining.join(", "),
                        elapsed
                    );
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    if emit {
        println!();
    }

    Ok((results, skipped))
}

/// Callback type for check events (used by TUI)
pub type CheckEventCallback = Box<dyn Fn(CheckEvent) + Send + Sync>;

/// Events emitted during check execution
#[derive(Debug, Clone)]
pub enum CheckEvent {
    Started { name: String },
    Completed { result: Box<CheckResult> },
    Skipped { name: String },
}

/// Run all applicable checks with event callbacks (for TUI mode)
pub async fn run_all_with_events<F>(
    config: &Config,
    ledger: &TaskLedger,
    on_event: F,
) -> Result<(Vec<CheckResult>, Vec<SkippedCheck>)>
where
    F: Fn(CheckEvent) + Send + Sync,
{
    let checks: Vec<Box<dyn Check>> = get_checks_for_profile(config);
    let cache = Cache::new(config);
    let mut results = Vec::new();
    let mut skipped = Vec::new();
    let mut runnable_checks: Vec<Box<dyn Check>> = Vec::new();

    // First pass: resolve skipped/cached checks and collect runnable ones.
    for check in checks {
        match check.check_eligibility(config) {
            CheckEligibility::Skip(reason) => {
                ledger.record(TaskEntry {
                    key: TaskKey::new(check.name(), ledger_substrate(None, ledger)),
                    kind: TaskKind::Check,
                    state: TaskState::Skipped {
                        reason: reason.clone(),
                    },
                    queued_at: None,
                    started_at: None,
                });
                let skipped_check = build_skipped_check(check.as_ref(), reason);
                let name = skipped_check.name.clone();
                skipped.push(skipped_check);
                on_event(CheckEvent::Skipped { name });
                continue;
            }
            CheckEligibility::Run => {}
        }

        if let Some(result) = load_cached_result(check.as_ref(), config, &cache) {
            let origin = ledger_substrate(result.provenance.as_ref(), ledger);
            ledger.record(TaskEntry {
                key: TaskKey::new(check.name(), origin.clone()),
                kind: TaskKind::Check,
                state: TaskState::Cached {
                    cache_age_secs: None,
                    origin,
                },
                queued_at: None,
                started_at: None,
            });
            on_event(CheckEvent::Completed {
                result: Box::new(result.clone()),
            });
            results.push(result);
            continue;
        }

        runnable_checks.push(check);
    }

    // Pre-sync Python venv before running checks, mirroring run_all behaviour.
    // This keeps venv build time outside the per-check timeout budget.
    let has_python_checks = runnable_checks
        .iter()
        .any(|c| matches!(c.name(), "Ruff" | "Mypy" | "Pytest"));
    if has_python_checks && config.profile.runs_python_checks() && which::which("uv").is_ok() {
        let _ = run_command_with_timeout(
            "uv",
            &["sync", "--quiet"],
            &config.repo_root,
            CHECK_TIMEOUT_SECS,
        )
        .await;
    }

    // Second pass: run checks in parallel, fire events as they complete.
    {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::sync::Arc;

        for check in &runnable_checks {
            on_event(CheckEvent::Started {
                name: check.name().to_string(),
            });
        }

        // One shared target snapshot for the whole run (thread 1); see run_all.
        let mut config = config.clone();
        let _shared_snapshot = share_target_snapshot(&mut config, &runnable_checks);

        let config = Arc::new(config);
        let cache = Arc::new(cache);
        // Cargo checks share one target/ build lock, so they serialize on a
        // single-permit semaphore while non-cargo checks stay parallel (PV-17).
        let cargo_lock = Arc::new(Semaphore::new(1));
        let mut futs: FuturesUnordered<_> = runnable_checks
            .into_iter()
            .map(|check| {
                let config = Arc::clone(&config);
                let cache = Arc::clone(&cache);
                let cargo_lock = Arc::clone(&cargo_lock);
                let queued_at = std::time::Instant::now();
                async move {
                    let started_at = std::time::Instant::now();
                    let _permit = if is_cargo_target_check(check.name()) {
                        Some(
                            cargo_lock
                                .acquire()
                                .await
                                .expect("cargo-target semaphore never closed"),
                        )
                    } else {
                        None
                    };
                    let result = execute_live_check(check, config.as_ref(), cache.as_ref()).await;
                    record_executed_check(&result, ledger, queued_at, started_at);
                    result
                }
            })
            .collect();

        while let Some(result) = futs.next().await {
            on_event(CheckEvent::Completed {
                result: Box::new(result.clone()),
            });
            results.push(result);
        }
    }

    Ok((results, skipped))
}

/// A skipped check identifies itself the way every other check does.
///
/// The id used to be a naive slug — a fourth copy of the normalisation
/// `crate::check_id` exists to be the canon of. It agrees with the alias table
/// for most names and silently disagrees for the aliased ones, so the same
/// configured gate appeared as `typescript` where it ran and as `tsc` where it
/// was skipped (likewise `cargo_check`/`cargo`, `vitest`/`tests`). Both ids
/// reach the artifacts — `REPORT.json.checks_skipped[]` and the skipped rows in
/// `PROVENANCE.json.checks[]` — so a consumer could not correlate a skip with
/// the gate it belongs to.
fn build_skipped_check(check: &dyn Check, reason: String) -> SkippedCheck {
    let name = check.name().to_string();
    let id = crate::check_id::check_id_from_name(&name);

    SkippedCheck { id, name, reason }
}

fn load_cached_result(check: &dyn Check, config: &Config, cache: &Cache) -> Option<CheckResult> {
    let cache_key = check.cache_key(config)?;
    let cached = cache.get(check.name(), &cache_key)?;
    let output = cached.output.unwrap_or_default();
    let mut status = cached.status.parse::<CheckStatus>().unwrap();

    if matches!(status, CheckStatus::Passed | CheckStatus::Warnings) && has_tool_crash(&output) {
        status = CheckStatus::Error;
    }

    Some(CheckResult {
        name: check.name().to_string(),
        status,
        duration: Duration::from_secs(0),
        output,
        cached: true,
        provenance: replayed_provenance(cached.provenance.as_deref()),
    })
}

/// Rebuild the provenance a cache hit is replaying.
///
/// The fastest runs are the cache hits, and they used to be the only ones with
/// no provenance at all — the audit trail went blank exactly where it was
/// cheapest to keep. The stored blob describes the ORIGINAL execution
/// (`started_at`/`finished_at`, `cwd`, `target_sha`, `tree_state` of the run
/// that populated the entry); `CheckResult::cached` is what marks the row as a
/// replay rather than a fresh execution.
///
/// Unreadable or absent blobs resolve to `None`: an entry written by an older
/// prview (no sidecar) or one whose schema has since changed replays with an
/// unknown provenance instead of panicking or failing the run.
fn replayed_provenance(stored: Option<&str>) -> Option<CheckProvenance> {
    serde_json::from_str(stored?).ok()
}

/// `CheckProvenance.command` for an execution that failed before any command
/// reported one (a timeout, a spawn failure). Explicit, so a consumer reading
/// the field sees an absence rather than a plausible-looking command line.
const NO_COMMAND_RECORDED: &str = "<no command recorded>";

/// The directory a check WOULD have read, for an execution that ended in `Err`.
///
/// Resolved WITHOUT materialising anything: the shared snapshot is already on
/// disk, and a review whose target is the checked-out `HEAD` reads the repo root.
/// An off-HEAD run with no shared snapshot returns `None` — that check built its
/// own worktree, which is gone by the time the error surfaces, and inventing a
/// path for it would put a fabricated substrate in the manifest.
///
/// A cargo check does not run at the scan root: a workspace member, or a crate
/// the reviewed commit moved, sits in a subdirectory of it. That resolution is
/// shared with `plan_cargo_run` rather than approximated here, so the manifest
/// names the directory the command was actually headed for.
fn errored_check_scan_dir(name: &str, config: &Config) -> Option<std::path::PathBuf> {
    let scan_dir = if let Some(scan_dir) = &config.scan_dir_override {
        uses_shared_scan_dir(name).then(|| scan_dir.clone())?
    } else {
        off_head_target_commit(config)
            .is_none()
            .then(|| config.repo_root.clone())?
    };
    Some(match is_cargo_target_check(name) {
        true => cargo::planned_cargo_cwd(config, &scan_dir),
        false => scan_dir,
    })
}

/// Provenance for a check that errored: no command, but the substrate it was
/// about to read.
///
/// A timeout or a crash produces exactly the rows a reviewer most needs to place
/// — "which tree produced this error" — and those were the rows the manifest
/// left entirely null.
fn errored_check_provenance(
    name: &str,
    config: &Config,
    cache_key: Option<String>,
    output: &str,
    started_at: String,
) -> Option<CheckProvenance> {
    let scan_dir = errored_check_scan_dir(name, config)?;
    Some(
        CheckProvenance {
            command: NO_COMMAND_RECORDED.to_string(),
            tool_version: None,
            cwd: scan_dir.display().to_string(),
            exit_code: None,
            started_at,
            finished_at: chrono::Local::now().to_rfc3339(),
            hard_fail_signatures: find_hard_fail_signatures(output),
            cache_key,
            target_sha: None,
            tree_state: None,
        }
        .with_scan_substrate(name, &scan_dir, &config.repo_root),
    )
}

/// Record a check that actually executed, keyed on the tree its own provenance
/// says it read.
///
/// `queued_at` is when the check entered the execution set and `started_at` when
/// its future first ran; today those differ only by scheduling latency, because
/// nothing yet holds a check back from starting. A governor that does will make
/// the split meaningful without moving these call sites.
fn record_executed_check(
    result: &CheckResult,
    ledger: &TaskLedger,
    queued_at: std::time::Instant,
    started_at: std::time::Instant,
) {
    ledger.record(TaskEntry {
        key: TaskKey::new(
            &result.name,
            ledger_substrate(result.provenance.as_ref(), ledger),
        ),
        kind: TaskKind::Check,
        state: TaskState::Run {
            duration: result.duration,
        },
        queued_at: Some(queued_at),
        started_at: Some(started_at),
    });
}

async fn execute_live_check(check: Box<dyn Check>, config: &Config, cache: &Cache) -> CheckResult {
    let start = std::time::Instant::now();
    let started_at = chrono::Local::now().to_rfc3339();
    let name = check.name().to_string();
    let cache_key = check.cache_key(config);

    match check.run(config).await {
        Ok(mut result) => {
            if matches!(result.status, CheckStatus::Passed | CheckStatus::Warnings)
                && has_tool_crash(&result.output)
            {
                result.status = CheckStatus::Error;
            }

            // Never cache a runtime Skipped result. A check that RAN but
            // skipped (mypy: uv "failed to spawn" a missing binary; geiger: a
            // virtual workspace manifest) reflects an environmental/transient
            // setup gap, not a stable property of the source. Caching it under
            // the source-hash key (e.g. `mypy-<python_hash>`) would pin the
            // transient miss for the whole hash lifetime, so a later run with
            // the tool present still reports Skipped (PR #12 review #14).
            if result.status != CheckStatus::Skipped
                && let Some(key) = cache_key.clone()
            {
                // Store the provenance next to the result so a later cache hit
                // can replay it. Serialization is best effort: a provenance that
                // cannot be encoded must never cost us the cached result itself.
                let provenance = result
                    .provenance
                    .as_ref()
                    .and_then(|prov| serde_json::to_string(prov).ok());
                if let Err(e) = cache.set(
                    &name,
                    &key,
                    result.status.as_str(),
                    Some(&result.output),
                    provenance.as_deref(),
                ) {
                    eprintln!("  warning: cache write failed for {name}: {e}");
                }
            }

            result
        }
        Err(e) => {
            let msg = e.to_string();
            // A missing/unlaunchable tool is a setup gap, not a quality failure,
            // so downgrade to Skipped to avoid poisoning the gate. EXCEPTION:
            // security tools stay loud (Error) — one that passed which::which()
            // at eligibility but then fails to spawn (broken/partial binary,
            // PATH change, TOCTOU) must not vanish silently.
            let status = if tool_unavailable_signature(&msg) && !is_security_check(&name) {
                CheckStatus::Skipped
            } else {
                CheckStatus::Error
            };
            let provenance = errored_check_provenance(&name, config, cache_key, &msg, started_at);
            CheckResult {
                name,
                status,
                duration: start.elapsed(),
                output: msg,
                cached: false,
                provenance,
            }
        }
    }
}

fn format_status(status: CheckStatus) -> String {
    use colored::Colorize;
    match status {
        CheckStatus::Passed => "✓".green().to_string(),
        CheckStatus::Failed => "✗".red().to_string(),
        CheckStatus::Warnings => "⚠".yellow().to_string(),
        CheckStatus::Skipped => "○".dimmed().to_string(),
        CheckStatus::Error => "!".red().to_string(),
    }
}

/// Get checks supported by the detected profile.
///
/// Individual checks decide whether they can execute in the current run via
/// `Check::can_run`, which preserves explicit opt-out flows while keeping
/// canonical status outputs complete.
pub fn get_checks_for_profile(config: &Config) -> Vec<Box<dyn Check>> {
    let mut checks: Vec<Box<dyn Check>> = Vec::new();

    // Security scans are applicable to all profiles
    checks.push(Box::new(SemgrepCheck));

    match config.profile.kind {
        ProfileKind::Js => {
            checks.push(Box::new(TypeScriptCheck));
            checks.push(Box::new(ESLintCheck));
            checks.push(Box::new(StylelintCheck));
            checks.push(Box::new(VitestCheck));
        }
        ProfileKind::Rust => {
            checks.push(Box::new(CargoCheck));
            checks.push(Box::new(ClippyCheck));
            checks.push(Box::new(RustfmtCheck));
            checks.push(Box::new(CargoTestCheck));
            checks.push(Box::new(CargoAuditCheck));
            if config.security_full {
                checks.push(Box::new(CargoGeigerCheck));
            }
        }
        ProfileKind::Python => {
            checks.push(Box::new(RuffCheck));
            checks.push(Box::new(MypyCheck));
            checks.push(Box::new(PytestCheck));
        }
        ProfileKind::Mixed => {
            if config.profile.has_tsconfig {
                checks.push(Box::new(TypeScriptCheck));
            }
            if config.profile.has_package_json {
                checks.push(Box::new(ESLintCheck));
                checks.push(Box::new(StylelintCheck));
                checks.push(Box::new(VitestCheck));
            }
            if config.profile.has_cargo {
                checks.push(Box::new(CargoCheck));
                checks.push(Box::new(ClippyCheck));
                checks.push(Box::new(RustfmtCheck));
                checks.push(Box::new(CargoTestCheck));
                checks.push(Box::new(CargoAuditCheck));
                if config.security_full {
                    checks.push(Box::new(CargoGeigerCheck));
                }
            }
            if config.profile.runs_python_checks() {
                checks.push(Box::new(RuffCheck));
                checks.push(Box::new(MypyCheck));
                checks.push(Box::new(PytestCheck));
            }
        }
        ProfileKind::Generic => {}
    }

    checks
}

/// Hard failure signature patterns (Artifact Pack v1 spec)
const HARD_FAIL_SIGNATURES: &[(&str, &str)] = &[
    // Rust
    ("thread '", "Rust panic"),
    // Node
    ("unhandledpromiserejection", "Node unhandled rejection"),
    ("err_unhandled_rejection", "Node unhandled rejection"),
    // Python
    ("traceback (most recent call last):", "Python traceback"),
    // General
    ("segmentation fault", "Segfault"),
    ("addresssanitizer", "ASan"),
    ("ubsan:", "UBSan"),
    ("threadsanitizer", "TSan"),
    ("sigabrt", "SIGABRT"),
    ("sigsegv", "SIGSEGV"),
    ("fatal runtime error", "Fatal runtime error"),
    ("stack overflow", "Stack overflow"),
];

/// Detect tool crash indicators in combined output
pub fn has_tool_crash(output: &str) -> bool {
    !find_hard_fail_signatures(output).is_empty()
}

/// Cargo checks share one target/ build lock, so serialize them (PV-17).
fn is_cargo_target_check(name: &str) -> bool {
    matches!(
        name,
        "Cargo check" | "Clippy" | "Rustfmt" | "Cargo test" | "Cargo audit" | "Cargo geiger"
    )
}

/// Checks that resolve their scan directory through [`plan_check_run`] and so
/// benefit from the run-wide shared target snapshot (thread 1).
///
/// The cargo checks are listed too: they analyse the reviewed snapshot like
/// every other language check and only redirect their *build cache* away from
/// it (see `plan_cargo_run` in `checks::cargo`). Semgrep is the single opt-out —
/// it manages its own worktree because it also needs a baseline commit.
fn uses_shared_scan_dir(name: &str) -> bool {
    matches!(
        name,
        "Ruff"
            | "Mypy"
            | "Pytest"
            | "TypeScript"
            | "ESLint"
            | "Vitest"
            | "Stylelint"
            | "Cargo check"
            | "Clippy"
            | "Rustfmt"
            | "Cargo test"
            | "Cargo audit"
            | "Cargo geiger"
    )
}

/// Commit id of the reviewed target when it differs from the checked-out `HEAD`.
///
/// `None` for an ordinary local review (target == `HEAD`) and whenever the repo
/// or its refs cannot be resolved — both keep the plain working-tree behaviour.
///
/// Cache keys need this INDEPENDENTLY of `config.scan_dir_override`: the cached-
/// result lookup runs in the dispatcher's first pass, BEFORE the shared snapshot
/// is materialised. A key derived from the scan dir alone would therefore read a
/// local-tree key and write a snapshot key — and, worse, a `--pr` run would hit
/// the entry a previous local run stored under that same local-tree key, serving
/// the local checkout's verdict as if it were the reviewed commit's.
pub fn off_head_target_commit(config: &Config) -> Option<String> {
    let repo = crate::git::Repository::open(&config.repo_root).ok()?;
    let target = repo.resolve_target(config).ok()?;
    let head = repo.head_commit_id().ok()?;
    (target.commit_id != head).then_some(target.commit_id)
}

/// Materialise ONE target snapshot for the whole run and point `config` at it, so
/// every snapshot-backed check reuses a single worktree instead of creating its
/// own (thread 1). Returns the snapshot handle for the caller to keep alive until
/// all checks finish; `None` (leaving `scan_dir_override` unset) when no runnable
/// check needs a snapshot, or when snapshot creation fails — in which case each
/// check falls back to resolving its own plan, the original per-check behaviour.
fn share_target_snapshot(
    config: &mut Config,
    runnable_checks: &[Box<dyn Check>],
) -> Option<crate::git::WorktreeSnapshot> {
    if !runnable_checks
        .iter()
        .any(|c| uses_shared_scan_dir(c.name()))
    {
        return None;
    }
    match plan_check_run(config) {
        Ok(plan) => {
            config.scan_dir_override = Some(plan.scan_dir.clone());
            plan._snapshot
        }
        Err(_) => None,
    }
}

/// Security checks stay loud: a spawn failure here is NOT downgraded to Skipped
/// (PV-01), so a broken or half-installed security tool can't silently vanish
/// from the gate. They pass which::which() at eligibility, but a runtime spawn
/// failure (broken/partial binary, PATH change, TOCTOU) must still surface.
fn is_security_check(name: &str) -> bool {
    matches!(name, "Semgrep scan" | "Cargo audit" | "Cargo geiger")
}

/// Find all matching hard failure signatures in output
pub fn find_hard_fail_signatures(output: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("test ") {
            continue;
        }

        let lower = trimmed.to_ascii_lowercase();
        for &(pattern, label) in HARD_FAIL_SIGNATURES {
            let matched = if pattern == "thread '" {
                lower.contains("panic") && lower.contains(pattern)
            } else {
                lower.contains(pattern)
            };

            if matched && !found.iter().any(|existing| existing == label) {
                found.push(label.to_string());
            }
        }
    }
    found
}

/// True when prview's OWN process-runner error string indicates the tool binary
/// could not be launched (ENOENT on spawn) — e.g.
/// "Failed to run mypy: No such file or directory (os error 2)". Safe ONLY for
/// prview-generated error strings, where a match reliably means a spawn failure.
/// Do NOT run this against raw tool output: a tool legitimately prints
/// "no such file or directory" in its own diagnostics — use
/// `tool_spawn_failure_in_output` for that case.
pub fn tool_unavailable_signature(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("no such file or directory")
        || lower.contains("failed to spawn")
        || lower.contains("command not found")
        || lower.contains("program not found")
        || lower.contains("cannot find the file specified")
}

/// True when RAW TOOL OUTPUT shows a launcher could not spawn the requested tool.
/// Matches only markers a tool never emits in its own diagnostics — uv's
/// "failed to spawn" and a shell "command not found". A bare
/// "no such file or directory" is deliberately NOT matched here: tools print it
/// in genuine diagnostics, and matching it would turn a real failure into an
/// invisible pass (a tool-output false positive).
pub fn tool_spawn_failure_in_output(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("failed to spawn") || lower.contains("command not found")
}

/// Build provenance from a command execution
pub struct ProvenanceBuilder<'a> {
    /// The check's own [`Check::name`] — decides which dependency links the
    /// substrate classification may hold against this run (see
    /// [`consumable_scaffolding`]).
    pub check: &'a str,
    pub cmd: &'a str,
    pub args: &'a [&'a str],
    pub cwd: &'a Path,
    /// Repo root of the run. Classifies the substrate `cwd` sits on, and is the
    /// base for the repo-relative `cwd` rendering.
    pub repo_root: &'a Path,
    /// The command's exit status, or `None` when it never produced one — a
    /// timeout, or a run abandoned after the tree had already been read. Absent
    /// is not the same as absent provenance: the substrate was still scanned,
    /// and a row of nulls would claim otherwise.
    pub exit_code: Option<i32>,
    pub combined_output: &'a str,
    pub started_at: &'a str,
    pub finished_at: &'a str,
    pub cache_key: Option<String>,
}

impl ProvenanceBuilder<'_> {
    /// Record `cwd` verbatim (absolute path as the command saw it).
    pub fn build(self) -> CheckProvenance {
        let cwd = self.cwd.display().to_string();
        self.build_with_cwd_display(cwd)
    }

    /// Record `cwd` relative to the repo root (`[external]/…` when the command
    /// ran outside it, e.g. in a target snapshot).
    pub fn build_repo_relative_cwd(self) -> CheckProvenance {
        let cwd =
            crate::paths::normalize_path_display(&self.cwd.display().to_string(), self.repo_root);
        self.build_with_cwd_display(cwd)
    }

    fn build_with_cwd_display(self, cwd_display: String) -> CheckProvenance {
        CheckProvenance {
            command: format!("{} {}", self.cmd, self.args.join(" ")),
            tool_version: None,
            cwd: cwd_display,
            exit_code: self.exit_code,
            started_at: self.started_at.to_string(),
            finished_at: self.finished_at.to_string(),
            hard_fail_signatures: find_hard_fail_signatures(self.combined_output),
            cache_key: self.cache_key,
            target_sha: None,
            tree_state: None,
        }
        .with_scan_substrate(self.check, self.cwd, self.repo_root)
    }
}

/// Marker embedded in a command-timeout error. Shared so callers that detect a
/// timeout (e.g. cargo geiger's Skipped downgrade) match against one source of
/// truth instead of coupling to free text that a reword could silently break.
const TIMEOUT_MARKER: &str = "timed out after";

/// Build the error for a command that exceeded its timeout.
fn timeout_error(cmd: &str, timeout_secs: u64) -> anyhow::Error {
    anyhow::anyhow!("{} {} {}s", cmd, TIMEOUT_MARKER, timeout_secs)
}

/// Whether an error is a command timeout produced by [`timeout_error`].
pub fn is_timeout_error(err: &anyhow::Error) -> bool {
    err.to_string().contains(TIMEOUT_MARKER)
}

/// Helper to run a command with timeout
pub async fn run_command(cmd: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    run_command_with_timeout(cmd, args, cwd, CHECK_TIMEOUT_SECS).await
}

/// Helper to run a command with custom timeout
pub async fn run_command_with_timeout(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    timeout_secs: u64,
) -> Result<Output> {
    run_command_with_timeout_and_env(cmd, args, cwd, timeout_secs, &[]).await
}

/// Helper to run a command with extra environment variables.
///
/// `env` is applied to the child ONLY — the parent process environment is never
/// mutated, so a per-check override (cargo's `CARGO_TARGET_DIR`) cannot leak
/// into concurrently running checks.
pub async fn run_command_with_env(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(String, String)],
) -> Result<Output> {
    run_command_with_timeout_and_env(cmd, args, cwd, CHECK_TIMEOUT_SECS, env).await
}

/// Helper to run a command with custom timeout and extra environment variables.
pub async fn run_command_with_timeout_and_env(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    timeout_secs: u64,
    env: &[(String, String)],
) -> Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args).current_dir(cwd);
    for (key, value) in env {
        command.env(key, value);
    }
    // Shared rails (stdin-null, kill_on_drop, own process group) + concurrent
    // output drain + group-SIGKILL on timeout live in crate::proc.
    crate::proc::run_capture_with_timeout(command, Duration::from_secs(timeout_secs), cmd, || {
        timeout_error(cmd, timeout_secs)
    })
    .await
}

/// Helper to run JS tools via pnpm or npx (with tool availability check)
pub async fn run_js_command(tool: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    run_js_command_with_timeout(tool, args, cwd, CHECK_TIMEOUT_SECS).await
}

/// Helper to run JS tools with custom timeout (for tests)
pub async fn run_js_command_with_timeout(
    tool: &str,
    args: &[&str],
    cwd: &Path,
    timeout_secs: u64,
) -> Result<Output> {
    // Build full args list
    let pnpm_args: Vec<&str> = std::iter::once("exec")
        .chain(std::iter::once(tool))
        .chain(args.iter().copied())
        .collect();

    // --no-install: a missing tool must fail fast and parseably, never reach
    // npm's interactive "Ok to proceed?" prompt (the --deep hang class).
    let npx_args: Vec<&str> = ["--no-install", tool]
        .into_iter()
        .chain(args.iter().copied())
        .collect();

    // Prefer a resolved local binary: a direct exec with no launcher, no npm
    // registry consult, and no prompt (PR #12 review #15/#17). Fall back to
    // pnpm exec, then npx --no-install, only when the tool is not installed
    // locally.
    if let Some(bin) = local_js_bin(tool, cwd) {
        let bin = bin.to_string_lossy().into_owned();
        run_command_with_timeout(&bin, args, cwd, timeout_secs).await
    } else if which::which("pnpm").is_ok() {
        run_command_with_timeout("pnpm", &pnpm_args, cwd, timeout_secs).await
    } else {
        run_command_with_timeout("npx", &npx_args, cwd, timeout_secs).await
    }
}

/// Resolve a JS tool to a directly-runnable local binary, bypassing npx.
///
/// `npx --no-install` still consults npm and, on some npm versions, can prompt
/// or hit the network; a resolved `node_modules/.bin/<tool>` is an unambiguous
/// local exec with neither. Returns None when the tool is not installed locally
/// (the caller then falls back to pnpm/npx) (PR #12 review #15/#17).
pub fn local_js_bin(tool: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    let bin = cwd.join("node_modules/.bin").join(tool);
    bin.exists().then_some(bin)
}

/// Check if a JS tool is available in node_modules
pub fn js_tool_available(tool: &str, cwd: &Path) -> bool {
    local_js_bin(tool, cwd).is_some()
}

/// A resolved plan for running a check.
pub struct CheckPlan {
    /// Directory to run the check command in.
    pub scan_dir: std::path::PathBuf,
    /// Ephemeral worktree snapshot, kept alive until the check finishes.
    pub _snapshot: Option<crate::git::WorktreeSnapshot>,
}

/// Plan check execution path: if we are in a remote/PR mode (meaning resolved target
/// commit is different from the checked-out HEAD commit), create an ephemeral worktree
/// snapshot of the target commit and run there. Otherwise, scan the working tree in place.
///
/// When the dispatcher has already materialised ONE shared snapshot for the run
/// (`config.scan_dir_override`), reuse its directory instead of creating a
/// per-check worktree. The dispatcher owns and keeps that snapshot alive, so the
/// returned plan carries no snapshot of its own — avoiding N concurrent
/// `git worktree add/remove` calls (one per check) that contend on the git index
/// lock and re-check-out the whole tree repeatedly.
pub fn plan_check_run(config: &Config) -> Result<CheckPlan> {
    if let Some(scan_dir) = &config.scan_dir_override {
        return Ok(CheckPlan {
            scan_dir: scan_dir.clone(),
            _snapshot: None,
        });
    }

    let repo_root = config.repo_root.clone();
    let repo = match crate::git::Repository::open(&repo_root) {
        Ok(repo) => repo,
        Err(_) => {
            return Ok(CheckPlan {
                scan_dir: repo_root,
                _snapshot: None,
            });
        }
    };

    let (Ok(target), Ok(head)) = (repo.resolve_target(config), repo.head_commit_id()) else {
        return Ok(CheckPlan {
            scan_dir: repo_root,
            _snapshot: None,
        });
    };

    if head == target.commit_id {
        return Ok(CheckPlan {
            scan_dir: repo_root,
            _snapshot: None,
        });
    }

    // Ephemeral worktree
    let snapshot = crate::git::create_worktree_snapshot(&repo_root, &target.commit_id)?;
    Ok(CheckPlan {
        scan_dir: snapshot.worktree_path.clone(),
        _snapshot: Some(snapshot),
    })
}

impl CheckResult {
    pub fn is_failure(&self) -> bool {
        matches!(self.status, CheckStatus::Failed | CheckStatus::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ExecutionMode;
    use crate::config::{Config, test_config, test_rust_profile};
    use std::time::Duration;

    #[test]
    fn every_emitted_status_is_in_the_vocabulary() {
        // The vocabulary is what the CLI reader and the contract validator both
        // measure a pack against, so it has to BE the writer's image. A variant
        // added to the enum and forgotten here would be emitted into artifacts
        // and then reported as unreadable by the very tool that wrote it.
        let variants = [
            CheckStatus::Passed,
            CheckStatus::Failed,
            CheckStatus::Warnings,
            CheckStatus::Skipped,
            CheckStatus::Error,
        ];
        for variant in variants {
            assert!(
                CheckStatus::EMITTED.contains(&variant.as_str()),
                "{:?} is emitted but missing from the vocabulary",
                variant
            );
        }
        for spelling in CheckStatus::EMITTED {
            assert!(
                variants.iter().any(|v| v.as_str() == spelling),
                "{spelling} is in the vocabulary but nothing emits it"
            );
        }
        assert_eq!(
            variants.len(),
            CheckStatus::EMITTED.len(),
            "one variant per spelling"
        );
    }

    fn rust_config(run_tests: bool, run_lint: bool, run_security: bool) -> Config {
        let mut config = test_config();
        config.profile = test_rust_profile(true);
        config.execution_mode = ExecutionMode::Standard;
        config.run_tests = run_tests;
        config.run_lint = run_lint;
        config.run_security = run_security;
        config.do_fetch = false;
        config.use_cache = false;
        config.create_zip = false;
        config
    }

    /// A gate must carry the same id whether it ran or was ruled out. The naive
    /// slug agreed with the alias table for most names and disagreed exactly
    /// where an alias exists, so the same configured check appeared under two
    /// ids in the same pack and no consumer could pair them.
    #[test]
    fn a_skipped_check_keeps_the_canonical_gate_id() {
        for (check, id) in [
            (&cargo::CargoCheck as &dyn Check, "cargo"),
            (&typescript::TypeScriptCheck, "tsc"),
            (&typescript::VitestCheck, "tests"),
        ] {
            let skipped = build_skipped_check(check, "tool missing".to_string());
            assert_eq!(
                skipped.id,
                id,
                "{} must be identified as the gate it is, not as a slug of its display name",
                check.name(),
            );
            assert_eq!(
                skipped.id,
                crate::check_id::check_id_from_name(check.name()),
                "the skipped id must come from the canonical mapper",
            );
        }
    }

    /// Minimal git fixture: an initialised repo with one commit, returning the
    /// temp dir and the commit id.
    fn repo_with_one_commit() -> (tempfile::TempDir, String) {
        use crate::git::cmd::git_cmd;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let run_git = |args: &[&str]| {
            let out = git_cmd()
                .args(args)
                .current_dir(root)
                .output()
                .expect("git command");
            assert!(out.status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.email", "prview@example.test"]);
        run_git(&["config", "user.name", "prview test"]);
        run_git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("tracked.txt"), "one\n").expect("write fixture");
        run_git(&["add", "tracked.txt"]);
        run_git(&["commit", "-q", "-m", "one"]);

        let out = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("rev-parse");
        assert!(out.status.success());
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        (tmp, sha)
    }

    #[test]
    fn scan_substrate_of_a_target_snapshot_names_the_snapshot_commit() {
        // A snapshot-backed check scans an ephemeral worktree of the reviewed
        // commit. Provenance must name THAT commit and mark the tree as a
        // snapshot — otherwise an artifact cannot prove which tree was scanned.
        let (repo, first) = repo_with_one_commit();
        let root = repo.path();

        let snapshot =
            crate::git::create_worktree_snapshot(root, &first).expect("worktree snapshot");
        let substrate = resolve_scan_substrate(
            &snapshot.worktree_path,
            root,
            consumable_scaffolding("TypeScript"),
        );

        assert_eq!(substrate.target_sha.as_deref(), Some(first.as_str()));
        assert_eq!(substrate.tree_state, Some(TreeState::Snapshot));
    }

    #[test]
    fn scan_substrate_of_a_clean_local_tree_is_head() {
        // Target == HEAD: the check reads the local tree in place. With nothing
        // uncommitted, the scanned bytes really are HEAD.
        let (repo, head) = repo_with_one_commit();
        let root = repo.path();

        let substrate = resolve_scan_substrate(root, root, consumable_scaffolding("TypeScript"));

        assert_eq!(substrate.target_sha.as_deref(), Some(head.as_str()));
        assert_eq!(substrate.tree_state, Some(TreeState::LocalClean));
    }

    #[test]
    fn scan_substrate_of_a_dirty_local_tree_is_flagged() {
        // The audit-critical case: the check ran on HEAD's working tree, but the
        // tree carried uncommitted edits, so the scanned bytes are NOT HEAD. The
        // provenance must say so rather than claim a clean HEAD scan.
        let (repo, head) = repo_with_one_commit();
        let root = repo.path();

        std::fs::write(root.join("tracked.txt"), "edited\n").expect("dirty the tree");
        let substrate = resolve_scan_substrate(root, root, consumable_scaffolding("TypeScript"));
        assert_eq!(substrate.target_sha.as_deref(), Some(head.as_str()));
        assert_eq!(substrate.tree_state, Some(TreeState::LocalDirty));

        // An untracked file is dirt too — `git status --porcelain` lists it.
        std::fs::write(root.join("tracked.txt"), "one\n").expect("restore");
        assert_eq!(
            resolve_scan_substrate(root, root, consumable_scaffolding("TypeScript")).tree_state,
            Some(TreeState::LocalClean),
            "restoring the tracked file must return the tree to clean",
        );
        std::fs::write(root.join("untracked.txt"), "new\n").expect("write untracked");
        assert_eq!(
            resolve_scan_substrate(root, root, consumable_scaffolding("TypeScript")).tree_state,
            Some(TreeState::LocalDirty),
        );
    }

    #[test]
    fn scan_substrate_of_a_mutated_snapshot_is_not_certified_clean() {
        // A snapshot is not immutable: a check can write into it (cargo
        // generating a `Cargo.lock` the repo does not track). `snapshot` means
        // "bytes exactly equal target_sha", so a worktree carrying generated
        // files must not keep that label just because it sits outside repo_root.
        let (repo, first) = repo_with_one_commit();
        let root = repo.path();

        let snapshot =
            crate::git::create_worktree_snapshot(root, &first).expect("worktree snapshot");
        assert_eq!(
            resolve_scan_substrate(
                &snapshot.worktree_path,
                root,
                consumable_scaffolding("TypeScript")
            )
            .tree_state,
            Some(TreeState::Snapshot),
            "a freshly materialised snapshot is exactly the commit",
        );

        std::fs::write(snapshot.worktree_path.join("Cargo.lock"), "# generated\n")
            .expect("write generated file");
        let substrate = resolve_scan_substrate(
            &snapshot.worktree_path,
            root,
            consumable_scaffolding("TypeScript"),
        );
        assert_eq!(substrate.target_sha.as_deref(), Some(first.as_str()));
        assert_eq!(
            substrate.tree_state,
            Some(TreeState::SnapshotDirty),
            "a command that wrote into the snapshot must not be recorded as a clean commit scan",
        );
    }

    #[test]
    fn scan_substrate_of_a_snapshot_records_borrowed_dependencies() {
        // prview symlinks node_modules/.venv into the snapshot itself. That is
        // the tool's own scaffolding, never part of the reviewed commit, so it
        // must not flip every JS/Python review to a dirty snapshot — the
        // reviewed SOURCE really is the target's.
        //
        // It is not an exact snapshot scan either: the linked dependencies are
        // the operator's, so tsc/ESLint/Vitest read a compiler, plugins and
        // types the target's lockfile may not pin at all. Certifying that as
        // `snapshot` is the overclaim this record exists to prevent.
        let (repo, first) = repo_with_one_commit();
        let root = repo.path();
        std::fs::create_dir(root.join("node_modules")).expect("node_modules");
        std::fs::write(root.join("node_modules/marker"), "dep\n").expect("dep file");

        let snapshot =
            crate::git::create_worktree_snapshot(root, &first).expect("worktree snapshot");
        if !snapshot.worktree_path.join("node_modules").exists() {
            // Symlinking is unix-only; nothing to assert elsewhere.
            return;
        }

        let tree_state = resolve_scan_substrate(
            &snapshot.worktree_path,
            root,
            consumable_scaffolding("TypeScript"),
        )
        .tree_state;
        assert_ne!(
            tree_state,
            Some(TreeState::SnapshotDirty),
            "prview's own dependency symlinks are not a modification of the reviewed tree",
        );
        assert_eq!(
            tree_state,
            Some(TreeState::SnapshotBorrowedDeps),
            "a snapshot whose dependencies came from the local checkout is not an exact scan",
        );
    }

    #[test]
    fn a_snapshot_is_only_borrowed_for_the_checks_that_read_the_link() {
        // The mirror of the overclaim above: a mixed repository links
        // `node_modules` into EVERY snapshot, but a cargo or Python command
        // resolves nothing through it. Labelling those runs
        // `snapshot-borrowed-deps` reports a mixed substrate that never
        // existed — a false claim in the other direction.
        let (repo, first) = repo_with_one_commit();
        let root = repo.path();
        std::fs::create_dir(root.join("node_modules")).expect("node_modules");
        std::fs::write(root.join("node_modules/marker"), "dep\n").expect("dep file");

        let snapshot =
            crate::git::create_worktree_snapshot(root, &first).expect("worktree snapshot");
        if !snapshot.worktree_path.join("node_modules").exists() {
            // Symlinking is unix-only; nothing to assert elsewhere.
            return;
        }

        let state = |check: &str| {
            resolve_scan_substrate(&snapshot.worktree_path, root, consumable_scaffolding(check))
                .tree_state
        };

        assert_eq!(
            state("TypeScript"),
            Some(TreeState::SnapshotBorrowedDeps),
            "tsc resolves its compiler and types through the linked tree",
        );
        for check in ["Cargo check", "Clippy", "Cargo test", "Semgrep"] {
            assert_eq!(
                state(check),
                Some(TreeState::Snapshot),
                "{check} reads nothing through node_modules, so its scan is exactly the commit",
            );
        }
        for check in ["Ruff", "Mypy", "Pytest"] {
            assert_eq!(
                state(check),
                Some(TreeState::Snapshot),
                "{check} runs against the per-commit UV_PROJECT_ENVIRONMENT, not a linked tree",
            );
        }
    }

    #[test]
    fn checks_that_install_nothing_locally_consume_no_scaffolding() {
        // Guards the classification table itself: the cargo, semgrep and Python
        // commands must stay out of the consumable list. The Python entry is the
        // load-bearing one — it is empty only because `plan_python_run`
        // redirects uv away from the linked `.venv`.
        for check in [
            "Cargo check",
            "Clippy",
            "Rustfmt",
            "Cargo test",
            "Cargo audit",
            "Cargo geiger",
            "Semgrep",
            "Ruff",
            "Mypy",
            "Pytest",
        ] {
            assert!(
                consumable_scaffolding(check).is_empty(),
                "{check} does not read prview's dependency links",
            );
        }
        for check in ["TypeScript", "ESLint", "Vitest", "Stylelint"] {
            assert_eq!(
                consumable_scaffolding(check),
                &["node_modules"],
                "{check} resolves its toolchain through node_modules",
            );
        }
    }

    #[test]
    fn a_snapshot_without_scaffolding_stays_an_exact_scan() {
        // The borrowed-deps state is about links that EXIST. A review of a repo
        // with no local dependency tree links nothing, so nothing was borrowed
        // and the snapshot is exactly the reviewed commit.
        let (repo, first) = repo_with_one_commit();
        let root = repo.path();
        let snapshot =
            crate::git::create_worktree_snapshot(root, &first).expect("worktree snapshot");

        assert_eq!(
            resolve_scan_substrate(
                &snapshot.worktree_path,
                root,
                consumable_scaffolding("TypeScript")
            )
            .tree_state,
            Some(TreeState::Snapshot),
        );
    }

    #[test]
    fn a_nested_checkout_inside_the_repo_is_foreign_too() {
        // Sitting lexically below repo_root does not make a directory this
        // repository's working tree: a vendored checkout (or a submodule) is a
        // repository of its own, and `discover` resolves to IT. Recording its
        // HEAD as `local-clean` states that the reviewed repository's tree is at
        // a commit that belongs to a different project entirely.
        let (repo, _head) = repo_with_one_commit();
        let root = repo.path();

        let (nested_src, nested_head) = repo_with_one_commit();
        let nested = root.join("vendor/other");
        std::fs::create_dir_all(nested.parent().expect("parent")).expect("vendor dir");
        copy_tree(nested_src.path(), &nested);

        let substrate = resolve_scan_substrate(&nested, root, &[]);
        assert_eq!(
            substrate.target_sha.as_deref(),
            Some(nested_head.as_str()),
            "the sha read is the nested repository's, which is exactly the problem",
        );
        assert_eq!(
            substrate.tree_state,
            Some(TreeState::Foreign),
            "another repository's tree says nothing about the reviewed commit, \
             wherever it happens to sit",
        );
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("create dir");
        for entry in std::fs::read_dir(from).expect("read dir") {
            let entry = entry.expect("entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("file type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).expect("copy file");
            }
        }
    }

    #[test]
    fn scan_substrate_of_a_foreign_checkout_is_not_a_snapshot() {
        // Being outside repo_root does NOT make a directory a snapshot of the
        // reviewed commit: an external cargo_root is a different repository
        // entirely. Labelling it `snapshot` would certify a foreign tree's
        // verdict as the reviewed commit's.
        let (repo, _) = repo_with_one_commit();
        let (other, other_head) = repo_with_one_commit();

        let substrate = resolve_scan_substrate(
            other.path(),
            repo.path(),
            consumable_scaffolding("TypeScript"),
        );
        assert_eq!(substrate.target_sha.as_deref(), Some(other_head.as_str()));
        assert_eq!(substrate.tree_state, Some(TreeState::Foreign));
    }

    #[test]
    fn unreadable_status_is_unknown_not_clean() {
        // An index lock, a permissions error or a malformed repository must not
        // be reported as "no uncommitted changes" — that would certify bytes
        // nobody could verify.
        let (repo, _) = repo_with_one_commit();
        let root = repo.path();
        std::fs::write(root.join(".git/index"), b"not an index at all").expect("corrupt the index");

        let opened = git2::Repository::discover(root).expect("repo still opens");
        assert_eq!(
            working_tree_is_dirty(&opened, &[]),
            None,
            "a status that cannot be read is unknown, not clean",
        );
        assert_eq!(
            resolve_scan_substrate(root, root, consumable_scaffolding("TypeScript")).tree_state,
            None,
            "an unknown substrate must stay visibly unknown in provenance",
        );
    }

    #[test]
    fn scan_substrate_outside_a_repository_stays_unknown() {
        // No git repo: report nothing rather than guessing a substrate.
        let tmp = tempfile::tempdir().expect("tempdir");
        if git2::Repository::discover(tmp.path()).is_ok() {
            // TMPDIR itself lives inside a repository on this machine — the
            // no-repo case cannot be staged here.
            return;
        }
        let substrate =
            resolve_scan_substrate(tmp.path(), tmp.path(), consumable_scaffolding("TypeScript"));
        assert_eq!(substrate, ScanSubstrate::default());
    }

    #[test]
    fn plan_check_run_reuses_shared_scan_dir_without_new_worktree() {
        // Once the dispatcher has set a run-wide shared snapshot, every check's
        // plan_check_run must reuse that directory and create NO worktree of its
        // own — so N snapshot-backed checks add 0 extra worktrees (thread 1).
        let shared = std::path::PathBuf::from("/tmp/prview-shared-snapshot");
        let mut config = test_config();
        config.scan_dir_override = Some(shared.clone());

        for _ in 0..6 {
            let plan = plan_check_run(&config).expect("override path never fails");
            assert_eq!(plan.scan_dir, shared);
            assert!(
                plan._snapshot.is_none(),
                "a shared override must be reused, never re-materialised as a new worktree",
            );
        }
    }

    #[test]
    fn pytest_shares_the_run_wide_target_snapshot() {
        // PRV-PYTEST-HEAD: Pytest resolves its cwd through plan_check_run like
        // every other Python/JS check, so it must be listed here — otherwise a
        // Python-only run would materialise a second worktree, and Pytest could
        // drift back to running against the local checkout.
        for name in ["Ruff", "Mypy", "Pytest", "Vitest", "Cargo test"] {
            assert!(
                uses_shared_scan_dir(name),
                "{name} resolves via plan_check_run and must share the run-wide snapshot",
            );
        }
        assert!(
            !uses_shared_scan_dir("Semgrep"),
            "semgrep manages its own worktree (needs a baseline commit) and must stay out",
        );
    }

    #[test]
    fn share_target_snapshot_is_a_noop_without_snapshot_backed_checks() {
        // Semgrep owns its own worktree, so a semgrep-only run creates no shared
        // worktree and the override stays unset.
        let mut config = rust_config(true, true, true);
        let semgrep_only: Vec<Box<dyn Check>> =
            vec![Box::new(crate::checks::semgrep::SemgrepCheck)];
        let snapshot = share_target_snapshot(&mut config, &semgrep_only);
        assert!(snapshot.is_none());
        assert!(config.scan_dir_override.is_none());
    }

    #[test]
    fn cargo_checks_are_snapshot_backed() {
        // Cargo checks judge the reviewed commit like every other language
        // check, so they must take part in the run-wide shared snapshot; only
        // their build cache is redirected away from it. Semgrep stays the one
        // opt-out because it manages its own baseline worktree.
        for name in [
            "Cargo check",
            "Clippy",
            "Rustfmt",
            "Cargo test",
            "Cargo audit",
            "Cargo geiger",
        ] {
            assert!(
                uses_shared_scan_dir(name),
                "{name} must resolve its scan dir through the shared snapshot",
            );
        }
        assert!(!uses_shared_scan_dir("Semgrep scan"));
    }

    #[test]
    fn test_check_status_from_str_passed() {
        assert_eq!(
            "passed".parse::<CheckStatus>().unwrap(),
            CheckStatus::Passed
        );
        assert_eq!(
            "PASSED".parse::<CheckStatus>().unwrap(),
            CheckStatus::Passed
        );
    }

    #[test]
    fn test_check_status_from_str_failed() {
        assert_eq!(
            "failed".parse::<CheckStatus>().unwrap(),
            CheckStatus::Failed
        );
    }

    #[test]
    fn test_check_status_from_str_warnings() {
        assert_eq!(
            "warnings".parse::<CheckStatus>().unwrap(),
            CheckStatus::Warnings
        );
    }

    #[test]
    fn test_check_status_from_str_skipped() {
        assert_eq!(
            "skipped".parse::<CheckStatus>().unwrap(),
            CheckStatus::Skipped
        );
    }

    #[test]
    fn test_check_status_from_str_unknown() {
        assert_eq!(
            "unknown".parse::<CheckStatus>().unwrap(),
            CheckStatus::Error
        );
    }

    #[test]
    fn test_check_status_as_str() {
        assert_eq!(CheckStatus::Passed.as_str(), "passed");
        assert_eq!(CheckStatus::Failed.as_str(), "failed");
        assert_eq!(CheckStatus::Warnings.as_str(), "warnings");
        assert_eq!(CheckStatus::Skipped.as_str(), "skipped");
        assert_eq!(CheckStatus::Error.as_str(), "error");
    }

    #[test]
    fn test_check_result_is_failure_failed() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(result.is_failure());
    }

    #[test]
    fn test_check_result_is_failure_error() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Error,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(result.is_failure());
    }

    #[test]
    fn is_timeout_error_matches_the_shared_constructor() {
        // The cargo geiger Skipped downgrade matches timeouts via is_timeout_error;
        // keep it coupled to the one constructor so a reword can't silently break
        // it. A rename on either side trips this test.
        assert!(is_timeout_error(&timeout_error("cargo", 600)));
        assert!(!is_timeout_error(&anyhow::anyhow!(
            "Failed to run cargo: No such file"
        )));
    }

    #[test]
    fn test_check_result_is_failure_passed() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure());
    }

    #[test]
    fn test_check_result_is_failure_warnings() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Warnings,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure());
    }

    #[test]
    fn test_check_result_is_failure_skipped() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Skipped,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure());
    }

    #[test]
    fn test_check_status_serialization() {
        let passed = CheckStatus::Passed;
        let serialized = serde_json::to_string(&passed).unwrap();
        assert_eq!(serialized, "\"passed\"");

        let failed = CheckStatus::Failed;
        let serialized = serde_json::to_string(&failed).unwrap();
        assert_eq!(serialized, "\"failed\"");
    }

    #[test]
    fn test_check_status_deserialization() {
        let passed: CheckStatus = serde_json::from_str("\"passed\"").unwrap();
        assert_eq!(passed, CheckStatus::Passed);

        let failed: CheckStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(failed, CheckStatus::Failed);
    }

    #[test]
    fn test_check_result_serialization() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(5),
            output: "output".to_string(),
            cached: true,
            provenance: None,
        };
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(serialized.contains("\"name\":\"test\""));
        assert!(serialized.contains("\"status\":\"passed\""));
        assert!(serialized.contains("\"cached\":true"));
    }

    #[test]
    fn test_check_result_clone() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "output".to_string(),
            cached: false,
            provenance: None,
        };
        let cloned = result.clone();
        assert_eq!(result.name, cloned.name);
        assert_eq!(result.status, cloned.status);
        assert_eq!(result.cached, cloned.cached);
    }

    #[test]
    fn test_check_event_started_clone() {
        let event = CheckEvent::Started {
            name: "test".to_string(),
        };
        let cloned = event.clone();
        match cloned {
            CheckEvent::Started { name } => assert_eq!(name, "test"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn test_check_event_skipped_clone() {
        let event = CheckEvent::Skipped {
            name: "test".to_string(),
        };
        let cloned = event.clone();
        match cloned {
            CheckEvent::Skipped { name } => assert_eq!(name, "test"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn test_check_event_completed_clone() {
        let result = CheckResult {
            name: "test".to_string(),
            status: CheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        let event = CheckEvent::Completed {
            result: Box::new(result.clone()),
        };
        let cloned = event.clone();
        match cloned {
            CheckEvent::Completed { result } => assert_eq!(result.name, "test"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn test_format_status_passed() {
        let status = format_status(CheckStatus::Passed);
        assert!(status.contains('✓') || status.contains("✓"));
    }

    #[test]
    fn test_format_status_failed() {
        let status = format_status(CheckStatus::Failed);
        assert!(status.contains('✗') || status.contains("✗"));
    }

    #[test]
    fn test_format_status_warnings() {
        let status = format_status(CheckStatus::Warnings);
        assert!(status.contains('⚠') || status.contains("⚠"));
    }

    #[test]
    fn test_format_status_skipped() {
        let status = format_status(CheckStatus::Skipped);
        assert!(status.contains('○') || status.contains("○"));
    }

    #[test]
    fn test_format_status_error() {
        let status = format_status(CheckStatus::Error);
        assert!(status.contains('!') || status.contains("!"));
    }

    #[test]
    fn test_get_checks_for_profile_keeps_rust_checks_visible_when_disabled() {
        let config = rust_config(false, false, false);
        let check_names: Vec<String> = get_checks_for_profile(&config)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();

        assert!(check_names.iter().any(|name| name == "Cargo check"));
        assert!(check_names.iter().any(|name| name == "Clippy"));
        assert!(check_names.iter().any(|name| name == "Cargo test"));
        // cargo geiger is opt-in via --security-full: cleanly absent from the
        // default profile, never a skipped-caveat.
        assert!(!check_names.iter().any(|name| name == "Cargo geiger"));
    }

    #[test]
    fn test_get_checks_for_profile_adds_geiger_with_security_full() {
        let mut config = rust_config(false, false, false);
        config.security_full = true;
        let check_names: Vec<String> = get_checks_for_profile(&config)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();

        assert!(check_names.iter().any(|name| name == "Cargo geiger"));
    }

    #[test]
    fn test_gate_profile_keeps_rust_gate_fast_and_geiger_out() {
        let mut config = rust_config(true, true, true);
        config.execution_mode = ExecutionMode::Deep;
        config.security_full = true;
        config.apply_gate_profile(crate::policy::engine::EnforcementMode::Advisory);

        let checks = get_checks_for_profile(&config);
        let check_names: Vec<String> = checks
            .iter()
            .map(|check| check.name().to_string())
            .collect();

        assert_eq!(config.execution_mode, ExecutionMode::Quick);
        assert!(!config.run_tests);
        assert!(!config.run_lint);
        assert!(!config.run_security);
        assert!(!config.run_heuristics);
        assert!(!config.security_full);

        assert!(check_names.iter().any(|name| name == "Cargo check"));
        assert!(check_names.iter().any(|name| name == "Clippy"));
        assert!(check_names.iter().any(|name| name == "Rustfmt"));
        assert!(check_names.iter().any(|name| name == "Cargo test"));
        assert!(check_names.iter().any(|name| name == "Cargo audit"));
        assert!(!check_names.iter().any(|name| name == "Cargo geiger"));

        let cargo_check = checks
            .iter()
            .find(|check| check.name() == "Cargo check")
            .expect("cargo check configured");
        assert!(matches!(
            cargo_check.check_eligibility(&config),
            CheckEligibility::Run
        ));

        for name in ["Clippy", "Rustfmt", "Cargo test", "Cargo audit"] {
            let check = checks
                .iter()
                .find(|check| check.name() == name)
                .expect("rust check configured");
            assert!(matches!(
                check.check_eligibility(&config),
                CheckEligibility::Skip(_)
            ));
        }
    }

    #[test]
    fn test_get_checks_for_profile_mixed_geiger_follows_security_full() {
        use crate::config::{DetectedProfile, ProfileKind, test_config_builder};
        use std::path::PathBuf;

        let mixed_cargo = || DetectedProfile {
            kind: ProfileKind::Mixed,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: true,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: Some(PathBuf::from(".")),
            rust_dirs: vec![PathBuf::from(".")],
            is_workspace: false,
        };

        // Default: geiger absent from a mixed cargo profile.
        let default_cfg = test_config_builder().profile(mixed_cargo()).build();
        let default_names: Vec<String> = get_checks_for_profile(&default_cfg)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();
        assert!(!default_names.iter().any(|name| name == "Cargo geiger"));

        // With --security-full: geiger joins the mixed profile.
        let full_cfg = test_config_builder()
            .profile(mixed_cargo())
            .security_full(true)
            .build();
        let full_names: Vec<String> = get_checks_for_profile(&full_cfg)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();
        assert!(full_names.iter().any(|name| name == "Cargo geiger"));
    }

    #[test]
    fn test_get_checks_for_profile_runs_python_for_pyproject_only_in_mixed_rust_repo() {
        // PR #12 review #11: a pyproject.toml is an explicit Python project
        // declaration, so a mixed Rust+Python repo that declares pyproject (even
        // without .py source yet) must run Ruff/Mypy/Pytest, not silently drop
        // them. (Reverses the earlier PV-05 "tooling-only pyproject does not
        // qualify" stance for the declared-project case.)
        use crate::config::{DetectedProfile, ProfileKind, test_config_builder};
        use std::path::PathBuf;

        let config = test_config_builder()
            .profile(DetectedProfile {
                kind: ProfileKind::Mixed,
                has_package_json: false,
                has_tsconfig: false,
                has_cargo: true,
                has_pyproject: true,
                has_python_source: false,
                has_js_source: false,
                cargo_root: Some(PathBuf::from(".")),
                rust_dirs: vec![PathBuf::from(".")],
                is_workspace: false,
            })
            .run_lint(true)
            .run_tests(true)
            .build();
        let check_names: Vec<String> = get_checks_for_profile(&config)
            .into_iter()
            .map(|check| check.name().to_string())
            .collect();

        assert!(check_names.iter().any(|name| name == "Cargo check"));
        assert!(check_names.iter().any(|name| name == "Ruff"));
        assert!(check_names.iter().any(|name| name == "Mypy"));
        assert!(check_names.iter().any(|name| name == "Pytest"));
    }

    #[test]
    fn test_has_tool_crash_rust_panic() {
        let output = "thread 'main' panicked at 'index out of bounds'";
        assert!(has_tool_crash(output));
    }

    #[test]
    fn test_has_tool_crash_segfault() {
        assert!(has_tool_crash("Segmentation fault (core dumped)"));
    }

    #[test]
    fn test_has_tool_crash_sigabrt() {
        assert!(has_tool_crash("Process received SIGABRT"));
    }

    #[test]
    fn test_has_tool_crash_stack_overflow() {
        assert!(has_tool_crash("fatal runtime error: stack overflow"));
    }

    #[test]
    fn test_has_tool_crash_clean_output() {
        assert!(!has_tool_crash("All checks passed successfully"));
    }

    #[test]
    fn test_has_tool_crash_panic_word_without_thread() {
        // "panic" alone without "thread '" should not trigger
        assert!(!has_tool_crash("Don't panic, everything is fine"));
    }

    #[test]
    fn test_has_tool_crash_ignores_test_harness_names() {
        let output = "\
test checks::tests::test_has_tool_crash_sigabrt ... ok
test checks::tests::test_has_tool_crash_rust_panic ... ok
test result: ok. 2 passed; 0 failed
";
        assert!(!has_tool_crash(output));
    }

    // ── PV-16: process-tree kill on timeout ─────────────────────────
    // The grandchild process-group kill is proven canonically in
    // crate::proc::tests; here we only guard that the checks public fn routes a
    // timeout through the shared helper (returns a timeout error, no hang).

    #[tokio::test]
    async fn test_run_command_with_timeout_public_fn_times_out() {
        // Public-fn shape: a long sleep with a 1s budget must return a timeout
        // error (not hang, not Ok).
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = run_command_with_timeout("sleep", &["30"], tmp.path(), 1).await;
        let err = result.expect_err("sleep 30 with 1s timeout must error");
        assert!(is_timeout_error(&err), "error should be a timeout: {err}");
    }

    #[tokio::test]
    async fn test_run_command_with_timeout_success_returns_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let output = run_command_with_timeout("echo", &["hello"], tmp.path(), 10)
            .await
            .expect("echo should succeed");
        assert!(output.status.success(), "echo should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello"),
            "stdout should contain hello: {stdout}"
        );
    }

    // ── PV-17: cargo-family serialization ───────────────────────────

    #[test]
    fn test_is_cargo_target_check() {
        for name in [
            "Cargo check",
            "Clippy",
            "Rustfmt",
            "Cargo test",
            "Cargo audit",
            "Cargo geiger",
        ] {
            assert!(
                is_cargo_target_check(name),
                "{name} should be a cargo check"
            );
        }
        for name in [
            "Semgrep scan",
            "Ruff",
            "Mypy",
            "Pytest",
            "TypeScript",
            "ESLint",
        ] {
            assert!(
                !is_cargo_target_check(name),
                "{name} should NOT be a cargo check"
            );
        }
    }

    #[tokio::test]
    async fn test_cargo_semaphore_serializes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // N tasks share Semaphore(1); each bumps a counter on acquire and
        // asserts the in-flight count never exceeds 1.
        let sem = Arc::new(Semaphore::new(1));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let sem = Arc::clone(&sem);
            let in_flight = Arc::clone(&in_flight);
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore never closed");
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Hold the permit briefly so overlap would be observable.
                tokio::time::sleep(Duration::from_millis(5)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.expect("task join");
        }

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "Semaphore(1) must never allow more than one cargo task at a time"
        );
    }

    // ── PV-01: missing tool => Skipped, not Failed ──────────────────

    #[test]
    fn test_tool_unavailable_signature_matches_runner_enoent() {
        // prview-generated runner-error string for a missing binary.
        let err = "Failed to run mypy: No such file or directory (os error 2)";
        assert!(tool_unavailable_signature(err));
    }

    #[test]
    fn test_tool_unavailable_signature_ignores_real_type_error() {
        let err = "src/x.py:3: error: Incompatible return value type";
        assert!(!tool_unavailable_signature(err));
    }

    #[test]
    fn test_tool_spawn_failure_in_output_matches_uv_marker() {
        let out =
            "error: Failed to spawn: `mypy`\n  Caused by: No such file or directory (os error 2)";
        assert!(tool_spawn_failure_in_output(out));
    }

    #[test]
    fn test_tool_spawn_failure_in_output_ignores_bare_enoent_in_diagnostics() {
        // P1 guard: a tool's own diagnostic mentioning "no such file or
        // directory" must NOT be read as a spawn failure (would be an invisible
        // pass). Only the unambiguous launcher marker counts.
        let out = "src/a.py:10: error: Cannot read file: No such file or directory";
        assert!(!tool_spawn_failure_in_output(out));
    }

    #[test]
    fn test_skipped_result_is_not_failure() {
        let result = CheckResult {
            name: "Mypy".to_string(),
            status: CheckStatus::Skipped,
            duration: Duration::from_secs(0),
            output: String::new(),
            cached: false,
            provenance: None,
        };
        assert!(!result.is_failure(), "Skipped must not count as a failure");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_js_command_prefers_local_node_modules_bin() {
        // PR #12 review #15/#17: a tool present in node_modules/.bin must be
        // executed DIRECTLY, never through npx (which can prompt / hit the
        // registry). Prove both the resolver and that run_js_command runs the
        // local bin (its output could only come from the local script).
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let bindir = tmp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let toolpath = bindir.join("faketool");
        // Close and fsync the write fd in its own scope BEFORE chmod + spawn, so
        // the file is fully flushed and no writable descriptor to it lingers in
        // this thread when execve runs (first half of the ETXTBSY hardening).
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&toolpath).expect("create faketool");
            f.write_all(b"#!/bin/sh\necho LOCAL_BIN_RAN\n")
                .expect("write faketool");
            f.sync_all().expect("sync faketool");
        }
        let mut perms = std::fs::metadata(&toolpath).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&toolpath, perms).unwrap();

        assert!(
            local_js_bin("faketool", tmp.path()).is_some(),
            "installed tool must resolve to a local bin"
        );
        assert!(
            local_js_bin("absent", tmp.path()).is_none(),
            "absent tool must not resolve (caller falls back to npx)"
        );

        // On Linux a parallel test's fork can transiently inherit the write fd to
        // this freshly written executable, so execve races with "Text file busy"
        // (os error 26). Retry the spawn a few times; the racing child exec's and
        // drops the inherited fd almost immediately.
        let mut output = None;
        for attempt in 0..8u32 {
            match run_js_command_with_timeout("faketool", &[], tmp.path(), 10).await {
                Ok(o) => {
                    output = Some(o);
                    break;
                }
                Err(e) if attempt < 7 && e.to_string().contains("os error 26") => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("local bin should run: {e}"),
            }
        }
        let output = output.expect("local bin should run within the ETXTBSY retry budget");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("LOCAL_BIN_RAN"),
            "run_js_command must exec the local bin directly, got: {stdout}"
        );
    }

    /// Every check a run considers must leave exactly one ledger entry, stating
    /// how it resolved. A gate that is missing from the ledger is a gate a later
    /// consumer would re-run instead of recognising as already done.
    #[tokio::test]
    async fn run_all_records_one_ledger_entry_per_check() {
        use crate::ledger::{SubstrateKey, TaskKey, TaskKind, TaskLedger, TaskState};
        use async_trait::async_trait;

        struct MockCheck {
            name: &'static str,
            eligibility: CheckEligibility,
            cache_key: Option<&'static str>,
        }

        #[async_trait]
        impl Check for MockCheck {
            fn name(&self) -> &str {
                self.name
            }
            fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
                self.eligibility.clone()
            }
            async fn run(&self, _config: &Config) -> Result<CheckResult> {
                Ok(CheckResult {
                    name: self.name.to_string(),
                    status: CheckStatus::Passed,
                    duration: Duration::from_millis(7),
                    output: "ok".to_string(),
                    cached: false,
                    provenance: None,
                })
            }
            fn cache_key(&self, _config: &Config) -> Option<String> {
                self.cache_key.map(str::to_string)
            }
        }

        let (repo, _head) = repo_with_one_commit();
        let mut config = rust_config(false, false, false);
        config.repo_root = repo.path().to_path_buf();
        config.quiet = true;

        // A pre-populated entry so the cache-hit path is exercised for real.
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let cache = Cache::with_dir(cache_dir.path().to_path_buf(), true);
        cache
            .set("Mock cached", "cached-key", "passed", Some("replay"), None)
            .expect("seed cache");

        let checks: Vec<Box<dyn Check>> = vec![
            Box::new(MockCheck {
                name: "Mock ran",
                eligibility: CheckEligibility::Run,
                cache_key: None,
            }),
            Box::new(MockCheck {
                name: "Mock cached",
                eligibility: CheckEligibility::Run,
                cache_key: Some("cached-key"),
            }),
            Box::new(MockCheck {
                name: "Mock skipped",
                eligibility: CheckEligibility::Skip("lint disabled".to_string()),
                cache_key: None,
            }),
        ];

        let ledger = TaskLedger::new();
        let (results, skipped) = run_all_checks(checks, cache, &config, &ledger)
            .await
            .expect("run_all_checks");

        // Behaviour is unchanged: the ledger is written alongside, not instead.
        assert_eq!(results.len(), 2, "one live result plus one cache replay");
        assert_eq!(skipped.len(), 1);

        let entries = ledger.entries();
        assert_eq!(entries.len(), 3, "exactly one ledger entry per check");
        assert!(
            entries.iter().all(|e| e.kind == TaskKind::Check),
            "every entry recorded here is a check"
        );

        // Nothing resolved a run-wide substrate in this cut, and no mock reports
        // provenance, so every key carries the honestly-unknown substrate.
        let key = |name: &str| TaskKey::new(name, SubstrateKey::default());

        let ran = ledger.lookup(&key("Mock ran")).expect("executed check");
        assert_eq!(
            ran.state,
            TaskState::Run {
                duration: Duration::from_millis(7)
            },
            "an executed check records the duration its result reports"
        );
        assert!(
            ran.queued_at.is_some() && ran.started_at.is_some(),
            "an executed check is queued and started"
        );

        let cached = ledger.lookup(&key("Mock cached")).expect("cached check");
        assert_eq!(
            cached.state,
            TaskState::Cached {
                cache_age_secs: None,
                origin: SubstrateKey::default(),
            }
        );

        let skipped_entry = ledger.lookup(&key("Mock skipped")).expect("skipped check");
        assert_eq!(
            skipped_entry.state,
            TaskState::Skipped {
                reason: "lint disabled".to_string()
            },
            "the skip reason is preserved verbatim"
        );
        assert!(
            skipped_entry.queued_at.is_none() && skipped_entry.started_at.is_none(),
            "a check that never entered the queue has no queue timestamps"
        );
    }

    #[tokio::test]
    async fn runtime_skipped_result_is_not_cached() {
        // PR #12 review #14: a check that RAN but returned Skipped (mypy when uv
        // "failed to spawn" a missing binary) must NOT be persisted, or the
        // transient miss is pinned under the source-hash key for the whole hash
        // lifetime and a later run with the tool present still reports Skipped.
        use async_trait::async_trait;

        struct MockCheck {
            status: CheckStatus,
        }

        #[async_trait]
        impl Check for MockCheck {
            fn name(&self) -> &str {
                "Mock"
            }
            fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
                CheckEligibility::Run
            }
            async fn run(&self, _config: &Config) -> Result<CheckResult> {
                Ok(CheckResult {
                    name: "Mock".to_string(),
                    status: self.status,
                    duration: Duration::from_secs(0),
                    output: "ok".to_string(),
                    cached: false,
                    provenance: None,
                })
            }
            fn cache_key(&self, _config: &Config) -> Option<String> {
                Some("mock-key".to_string())
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = rust_config(true, true, true);

        // A runtime Skipped result must not land in the cache.
        let cache = Cache::with_dir(tmp.path().to_path_buf(), true);
        let _ = execute_live_check(
            Box::new(MockCheck {
                status: CheckStatus::Skipped,
            }),
            &config,
            &cache,
        )
        .await;
        assert!(
            cache.get("Mock", "mock-key").is_none(),
            "a runtime Skipped result must not be cached"
        );

        // Control: a Passed result IS cached, proving caching still works.
        let cache2 = Cache::with_dir(tmp.path().to_path_buf(), true);
        let _ = execute_live_check(
            Box::new(MockCheck {
                status: CheckStatus::Passed,
            }),
            &config,
            &cache2,
        )
        .await;
        assert!(
            cache2.get("Mock", "mock-key").is_some(),
            "a Passed result must still be cached"
        );
    }

    #[tokio::test]
    async fn a_check_that_errors_still_records_the_tree_it_was_reading() {
        // A command that times out (or crashes) returns `Err`, and the error
        // arm built a result with `provenance: None` — so the manifest emitted a
        // row whose cwd, target_sha and tree_state were all null. Those are the
        // rows that most need placing: "which tree produced this error" is the
        // first question asked about a timeout.
        use async_trait::async_trait;

        struct TimingOutCheck;

        #[async_trait]
        impl Check for TimingOutCheck {
            fn name(&self) -> &str {
                "Ruff"
            }
            fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
                CheckEligibility::Run
            }
            async fn run(&self, _config: &Config) -> Result<CheckResult> {
                anyhow::bail!("ruff {TIMEOUT_MARKER} 300s")
            }
            fn cache_key(&self, _config: &Config) -> Option<String> {
                Some("ruff-key".to_string())
            }
        }

        let (repo, head) = repo_with_one_commit();
        let mut config = rust_config(true, true, true);
        config.repo_root = repo.path().to_path_buf();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = Cache::with_dir(tmp.path().to_path_buf(), true);

        let result = execute_live_check(Box::new(TimingOutCheck), &config, &cache).await;
        assert_eq!(result.status, CheckStatus::Error);

        let prov = result
            .provenance
            .expect("an errored check must still say which tree it was reading");
        assert_eq!(prov.cwd, repo.path().display().to_string());
        assert_eq!(prov.target_sha.as_deref(), Some(head.as_str()));
        assert_eq!(prov.tree_state, Some(TreeState::LocalClean));
        assert_eq!(
            prov.exit_code, None,
            "no command completed, so there is no exit code to report",
        );
        assert_eq!(
            prov.command, NO_COMMAND_RECORDED,
            "the absence of a command must be stated, not invented",
        );
    }

    #[tokio::test]
    async fn an_errored_cargo_check_reports_the_member_it_was_headed_for() {
        // Reconstructing the substrate from the shared scan dir is right for a
        // check that runs at the snapshot root, but a cargo workspace member
        // runs one directory down. Collapsing every cargo run to the root
        // reported a directory the command never entered.
        use async_trait::async_trait;

        struct TimingOutCargo;

        #[async_trait]
        impl Check for TimingOutCargo {
            fn name(&self) -> &str {
                "Cargo check"
            }
            fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
                CheckEligibility::Run
            }
            async fn run(&self, _config: &Config) -> Result<CheckResult> {
                anyhow::bail!("cargo {TIMEOUT_MARKER} 600s")
            }
            fn cache_key(&self, _config: &Config) -> Option<String> {
                None
            }
        }

        let write_member = |root: &std::path::Path| {
            let member = root.join("crates/core");
            std::fs::create_dir_all(member.join("src")).expect("member");
            std::fs::write(
                member.join("Cargo.toml"),
                "[package]\nname = \"m\"\nversion = \"0.0.0\"\n",
            )
            .expect("manifest");
        };

        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
        write_member(repo_root.path());
        write_member(scan_dir.path());

        let mut config = rust_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(repo_root.path().join("crates/core"));
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = Cache::with_dir(tmp.path().to_path_buf(), true);
        let result = execute_live_check(Box::new(TimingOutCargo), &config, &cache).await;

        let prov = result
            .provenance
            .expect("an errored cargo check keeps its substrate");
        assert_eq!(
            prov.cwd,
            scan_dir.path().join("crates/core").display().to_string(),
            "the member the command was headed for, not the snapshot root",
        );
    }

    #[tokio::test]
    async fn an_off_head_check_that_errors_does_not_invent_a_snapshot_path() {
        // The honest limit of the above: off-HEAD with no shared snapshot, the
        // check built its own worktree and it is gone by the time the error
        // surfaces. An unknown substrate stays unknown rather than being
        // guessed as the local checkout — which is the one tree it was NOT
        // reading.
        use async_trait::async_trait;

        struct TimingOutCheck;

        #[async_trait]
        impl Check for TimingOutCheck {
            fn name(&self) -> &str {
                "Ruff"
            }
            fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
                CheckEligibility::Run
            }
            async fn run(&self, _config: &Config) -> Result<CheckResult> {
                anyhow::bail!("ruff {TIMEOUT_MARKER} 300s")
            }
            fn cache_key(&self, _config: &Config) -> Option<String> {
                None
            }
        }

        let (repo, head) = repo_with_one_commit();
        std::fs::write(repo.path().join("tracked.txt"), "two\n").expect("write");
        let run_git = |args: &[&str]| {
            let out = crate::git::cmd::git_cmd()
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("git command");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run_git(&["commit", "-qam", "two", "--no-verify"]);

        let mut config = rust_config(true, true, true);
        config.repo_root = repo.path().to_path_buf();
        config.target = Some(head);
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = Cache::with_dir(tmp.path().to_path_buf(), true);

        let result = execute_live_check(Box::new(TimingOutCheck), &config, &cache).await;
        assert!(
            result.provenance.is_none(),
            "a vanished per-check worktree must not be reported as the local tree",
        );
    }

    #[tokio::test]
    async fn cache_hit_replays_the_provenance_of_the_run_that_filled_it() {
        // Two passes over the dispatcher's own path: pass 1 executes the check
        // and writes the cache entry, pass 2 is served from cache. The cached
        // pass used to return `provenance: None`, so the FASTEST runs were the
        // ones with no audit trail at all.
        use async_trait::async_trait;

        struct MockCheck;

        #[async_trait]
        impl Check for MockCheck {
            fn name(&self) -> &str {
                "Mock"
            }
            fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
                CheckEligibility::Run
            }
            async fn run(&self, _config: &Config) -> Result<CheckResult> {
                Ok(CheckResult {
                    name: "Mock".to_string(),
                    status: CheckStatus::Passed,
                    duration: Duration::from_secs(0),
                    output: "ok".to_string(),
                    cached: false,
                    provenance: Some(CheckProvenance {
                        command: "mock --run".to_string(),
                        tool_version: Some("1.2.3".to_string()),
                        cwd: "[external]/snapshot".to_string(),
                        target_sha: Some("cafebabe".to_string()),
                        tree_state: Some(TreeState::Snapshot),
                        exit_code: Some(0),
                        started_at: "2026-08-22T10:00:00+02:00".to_string(),
                        finished_at: "2026-08-22T10:00:01+02:00".to_string(),
                        hard_fail_signatures: vec![],
                        cache_key: Some("mock-key".to_string()),
                    }),
                })
            }
            fn cache_key(&self, _config: &Config) -> Option<String> {
                Some("mock-key".to_string())
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = rust_config(true, true, true);
        let cache = Cache::with_dir(tmp.path().to_path_buf(), true);

        // Pass 1 — live execution, fills the cache.
        let live = execute_live_check(Box::new(MockCheck), &config, &cache).await;
        assert!(!live.cached);
        let live_prov = live.provenance.expect("live run must carry provenance");

        // Pass 2 — served from cache.
        let hit = load_cached_result(&MockCheck, &config, &cache)
            .expect("second pass must hit the cache");
        assert!(hit.cached, "a replay must announce itself as cached");
        let hit_prov = hit
            .provenance
            .expect("a cache hit must carry the provenance of the run that filled it");

        assert_eq!(hit_prov.command, live_prov.command);
        assert_eq!(hit_prov.cwd, live_prov.cwd);
        assert_eq!(hit_prov.target_sha, live_prov.target_sha);
        assert_eq!(hit_prov.tree_state, live_prov.tree_state);
        assert_eq!(hit_prov.started_at, live_prov.started_at);
        assert_eq!(hit_prov.exit_code, live_prov.exit_code);
        assert_eq!(hit_prov.cache_key, live_prov.cache_key);
    }

    #[test]
    fn legacy_cache_entry_without_provenance_replays_without_panicking() {
        // Entries written by an older prview have no provenance sidecar, and a
        // schema drift can leave one unparseable. Both must degrade to "unknown
        // provenance", never to a failed run.
        assert!(replayed_provenance(None).is_none());
        assert!(replayed_provenance(Some("{ not json")).is_none());
        assert!(replayed_provenance(Some(r#"{"unrelated":true}"#)).is_none());
    }
}
