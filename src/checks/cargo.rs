//! Rust/Cargo checks

use super::{
    Check, CheckResult, CheckStatus, ProvenanceBuilder, TEST_TIMEOUT_SECS, has_tool_crash,
    off_head_target_commit, plan_check_run, run_command_with_env, run_command_with_timeout_and_env,
};
use crate::Config;
use crate::cache;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use std::path::{Path, PathBuf};

pub struct CargoCheck;
pub struct ClippyCheck;
pub struct CargoTestCheck;
pub struct RustfmtCheck;
pub struct CargoAuditCheck;
pub struct CargoGeigerCheck;

/// The cargo package/workspace root inside the LOCAL checkout.
fn cargo_cache_root(config: &Config) -> &Path {
    config
        .profile
        .cargo_root
        .as_deref()
        .unwrap_or(config.repo_root.as_path())
}

/// Where a cargo check must execute, plus the environment it needs there.
struct CargoRun {
    /// Directory to run `cargo` in — the reviewed snapshot's cargo root in
    /// `--pr`/`--remote` mode, the local cargo root otherwise.
    cwd: PathBuf,
    /// Extra child environment (`CARGO_TARGET_DIR`), empty for a local run.
    env: Vec<(String, String)>,
    /// Ephemeral snapshot, kept alive until the check finishes.
    _snapshot: Option<crate::git::WorktreeSnapshot>,
}

/// Resolve where the cargo commands for this run must execute.
///
/// Cargo checks must judge the REVIEWED commit like every other language check.
/// Running them at the local cargo root meant that a `--pr`/`--remote` pack
/// combined the target's diff with build/clippy/test/fmt results from whatever
/// branch happened to be checked out locally — a foreign tree's verdict printed
/// under the reviewed PR's name (the 2026-07-24 remote-only regression).
///
/// Two cases:
/// - local review (target == `HEAD`, or the repo/refs cannot be resolved):
///   unchanged — the local cargo root, no environment override, so the
///   operator's own warm `target/` is used and left exactly as it was;
/// - reviewed review (target != `HEAD`): the matching cargo root INSIDE the
///   snapshot, with `CARGO_TARGET_DIR` pointed at the per-repo shared build
///   cache. Without that redirect every run would compile the entire dependency
///   graph from zero, because the snapshot is a fresh temp dir thrown away at
///   the end of the run — which is the reason the checks were pinned to the
///   local root in the first place.
///
/// A cargo root configured OUTSIDE the repo has no third case: a snapshot of
/// this repo can never contain it, so the reviewed tree cannot be analysed at
/// all. That combination is refused by [`CargoCheck::check_eligibility`] and
/// friends before a command is built; reaching it here is a bug, and the error
/// is loud rather than a silent fallback onto the operator's local tree.
fn plan_cargo_run(config: &Config) -> Result<CargoRun> {
    let local_root = cargo_cache_root(config).to_path_buf();
    let plan = plan_check_run(config)?;

    if plan.scan_dir == config.repo_root {
        return Ok(CargoRun {
            cwd: local_root,
            env: Vec::new(),
            _snapshot: plan._snapshot,
        });
    }

    let Some(mapped) = snapshot_cargo_root(&local_root, &config.repo_root, &plan.scan_dir) else {
        anyhow::bail!(
            "cargo root {} lies outside the repository, so the reviewed commit's tree cannot be \
             analysed; configure a cargo_root inside the repo or review the local checkout",
            local_root.display()
        );
    };

    // Cargo creates the target directory itself, so nothing is materialised
    // here — resolving the path stays free of filesystem side effects.
    let target_dir = config.cargo_build_cache_dir();

    let cwd = match resolve_reviewed_cargo_root(config) {
        // The reviewed commit's own tree said where its manifest is.
        ReviewedCargoRoot::Resolved(relative) => relative
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(plan.scan_dir.clone(), |acc, part| acc.join(part)),
        // Eligibility skips this case, so reaching it means a check ran anyway.
        ReviewedCargoRoot::Unavailable(reason) => anyhow::bail!(
            "the reviewed commit has no cargo root to run in ({reason}), so no cargo verdict can \
             be earned for it"
        ),
        // Git could not answer (an injected scan dir, an unreadable repo): fall
        // back to inspecting the materialised snapshot.
        ReviewedCargoRoot::Unknown => reviewed_cargo_root(mapped, &plan.scan_dir),
    };
    let cwd = contained_in_snapshot(cwd, &plan.scan_dir)?;

    Ok(CargoRun {
        cwd,
        env: vec![(
            "CARGO_TARGET_DIR".to_string(),
            target_dir.display().to_string(),
        )],
        _snapshot: plan._snapshot,
    })
}

/// Refuse a cargo root that leaves the reviewed snapshot.
///
/// The lexical check in [`repo_relative_cargo_root`] only sees the path the
/// OPERATOR configured. The reviewed commit controls the tree, and a directory
/// it replaced with a symlink to somewhere else resolves to a path with no `..`
/// in it — so cargo would run on a foreign tree and the verdict would be cached
/// under the reviewed commit, exactly the hole the external-root refusal closed.
/// Containment is therefore settled on the real, resolved paths, after the
/// snapshot exists.
///
/// A path that cannot be canonicalised (nothing there yet) is left alone: cargo
/// reporting a missing directory is a truthful local failure, not a foreign
/// tree's verdict. The path itself is returned unchanged — canonicalisation is
/// the test, not the answer, and resolving it would rewrite the directory
/// provenance reports (`/var` → `/private/var` on macOS).
fn contained_in_snapshot(cwd: PathBuf, scan_dir: &Path) -> Result<PathBuf> {
    let (Ok(resolved), Ok(root)) = (cwd.canonicalize(), scan_dir.canonicalize()) else {
        return Ok(cwd);
    };
    if !resolved.starts_with(&root) {
        anyhow::bail!(
            "cargo root {} resolves outside the reviewed snapshot ({}), so a verdict earned there \
             would describe another tree",
            cwd.display(),
            scan_dir.display(),
        );
    }
    Ok(cwd)
}

/// Validate the mapped cargo root against the reviewed snapshot.
///
/// `config.profile.cargo_root` describes the LOCAL checkout. When the reviewed
/// branch moved the crate (a root crate pushed into `backend/`, a member renamed)
/// that path does not exist in the snapshot, and projecting it blindly makes
/// cargo fail on a missing manifest — an execution error reported as the reviewed
/// crate's verdict. Fall back to the snapshot root when it carries a manifest of
/// its own: a workspace root still checks its members, and provenance records the
/// directory actually used. With no manifest anywhere, keep the mapped path so
/// cargo's own error names the real problem instead of prview inventing one.
fn reviewed_cargo_root(mapped: PathBuf, scan_dir: &Path) -> PathBuf {
    if mapped.join("Cargo.toml").exists() || !scan_dir.join("Cargo.toml").exists() {
        return mapped;
    }
    scan_dir.to_path_buf()
}

/// Map the local cargo root onto the same relative location inside `scan_dir`.
///
/// Returns `None` when the cargo root is not inside the repo, which a snapshot
/// of that repo can never contain.
fn snapshot_cargo_root(local_root: &Path, repo_root: &Path, scan_dir: &Path) -> Option<PathBuf> {
    let relative = repo_relative_cargo_root(local_root, repo_root)?;
    Some(
        relative
            .components()
            .fold(scan_dir.to_path_buf(), |acc, c| acc.join(c)),
    )
}

/// The cargo root as a path relative to the repo root — the discriminator that
/// says WHICH tree inside the reviewed commit cargo will read.
///
/// `None` when the configured root is not inside the repo (an absolute path
/// elsewhere, or one escaping through `..`).
fn repo_relative_cargo_root(local_root: &Path, repo_root: &Path) -> Option<PathBuf> {
    // A relative cargo root (`.`) is repo-root-relative by construction.
    let relative = if local_root.is_relative() {
        local_root
    } else {
        local_root.strip_prefix(repo_root).ok()?
    };

    if !relative.components().all(|c| {
        matches!(
            c,
            std::path::Component::CurDir | std::path::Component::Normal(_)
        )
    }) {
        return None;
    }

    Some(
        relative
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect(),
    )
}

/// Skip reason when the reviewed commit's cargo tree cannot be reached at all.
///
/// A `cargo_root` outside the repository cannot be materialised from a snapshot
/// of that repository. The pre-fix code quietly ran cargo at the local path
/// instead — scanning a tree the review does not describe and filing the verdict
/// under the reviewed commit. The honest answer is no verdict: skip with a reason
/// the pack records, rather than a green light earned by a different tree.
fn unreachable_reviewed_cargo_root(config: &Config) -> Option<String> {
    let commit = off_head_target_commit(config)?;
    let local_root = cargo_cache_root(config);
    if repo_relative_cargo_root(local_root, &config.repo_root).is_some() {
        return None;
    }
    Some(format!(
        "cargo root {} is outside the repo — commit {} cannot be checked there",
        local_root.display(),
        &commit[..commit.len().min(8)]
    ))
}

/// Where the reviewed commit keeps the cargo project this run must judge.
#[derive(Debug, PartialEq, Eq)]
enum ReviewedCargoRoot {
    /// Repo-relative directory inside the reviewed tree (empty = repo root).
    Resolved(String),
    /// The reviewed commit offers no single cargo root to run in; the string is
    /// the skip reason recorded in the pack.
    Unavailable(String),
    /// The question does not apply or could not be asked: a local review, or a
    /// repository git cannot read.
    Unknown,
}

/// How far below the repo root a moved manifest is looked for.
///
/// A crate that moved is moved a level or two (`backend/`, `crates/core`), and
/// every extra level widens the chance of matching an unrelated fixture crate
/// deep in the tree. Two levels covers the realistic moves; anything further is
/// treated as "not found" rather than guessed at.
const CARGO_ROOT_DISCOVERY_DEPTH: usize = 2;

/// Resolve the cargo root from the REVIEWED commit's tree.
///
/// Eligibility used to read `config.profile`, which describes the LOCAL
/// checkout: a branch that dropped or moved its last `Cargo.toml` was still
/// reviewed from a Rust checkout, so the cargo gates ran and filed cargo's own
/// "could not find `Cargo.toml`" as the reviewed commit's verdict — a failure
/// invented by the tool.
///
/// The reviewed tree IS the target commit's tree, so the question is answered
/// from git and no snapshot is materialised to ask it. Three candidates, in the
/// order [`reviewed_cargo_root`] would try them:
///
/// 1. the mapped cargo root — the local root at the same relative path;
/// 2. the repo root — a workspace root still checks its members;
/// 3. exactly one directory within [`CARGO_ROOT_DISCOVERY_DEPTH`] carrying a
///    manifest, for a crate the reviewed commit moved somewhere else.
///
/// Several candidates in step 3 resolve to nothing: which crate the review is
/// about is not a thing to guess. Walking the tree also settles containment for
/// free — git trees are not traversed through symlinks, so a root the reviewed
/// commit replaced with a symlink to an external directory has no entries here
/// and simply does not resolve.
fn resolve_reviewed_cargo_root(config: &Config) -> ReviewedCargoRoot {
    let (Some(commit), Some(relative)) = (
        off_head_target_commit(config),
        repo_relative_cargo_root(cargo_cache_root(config), &config.repo_root),
    ) else {
        return ReviewedCargoRoot::Unknown;
    };
    let Ok(repo) = crate::git::Repository::open(&config.repo_root) else {
        return ReviewedCargoRoot::Unknown;
    };

    let mapped = cargo_root_path(&relative);
    for candidate in [mapped.as_str(), ""] {
        let path = if candidate.is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{candidate}/Cargo.toml")
        };
        match repo.path_exists_at_commit(&commit, &path) {
            Ok(true) => return ReviewedCargoRoot::Resolved(candidate.to_string()),
            Ok(false) => {}
            // The question could not be asked — do not answer it.
            Err(_) => return ReviewedCargoRoot::Unknown,
        }
    }

    let Ok(moved) =
        repo.dirs_containing_at_commit(&commit, "Cargo.toml", CARGO_ROOT_DISCOVERY_DEPTH)
    else {
        return ReviewedCargoRoot::Unknown;
    };
    let short = &commit[..commit.len().min(8)];
    let aimed_at = if mapped.is_empty() {
        "the repo root".to_string()
    } else {
        mapped
    };
    match moved.as_slice() {
        [only] => ReviewedCargoRoot::Resolved(only.clone()),
        [] => ReviewedCargoRoot::Unavailable(format!(
            "commit {short} has no Cargo.toml at {aimed_at} or the repo root — not a cargo project",
        )),
        many => ReviewedCargoRoot::Unavailable(format!(
            "commit {short} has no Cargo.toml at {aimed_at}, and several elsewhere ({}) — \
             cannot tell which crate this review is about",
            many.join(", "),
        )),
    }
}

/// Skip reason when the reviewed commit offers no cargo root to run in.
fn missing_reviewed_cargo_manifest(config: &Config) -> Option<String> {
    match resolve_reviewed_cargo_root(config) {
        ReviewedCargoRoot::Unavailable(reason) => Some(reason),
        ReviewedCargoRoot::Resolved(_) | ReviewedCargoRoot::Unknown => None,
    }
}

/// Content hash for dependency-sensitive cargo checks (check/clippy/geiger).
///
/// `rust_hash` keys on files under `cargo_cache_root`. When that root is a
/// workspace member distinct from the repo root, Cargo still resolves the
/// dependency set from the workspace-root `Cargo.lock` — and a member usually
/// has no lockfile of its own. Hashing only member files therefore lets a
/// root-lockfile-only dependency bump reuse a stale cached result. Fold the
/// repo-root lockfile in whenever the cargo root differs from the repo root so
/// such a bump invalidates the member key.
fn cargo_content_hash(config: &Config) -> String {
    if let Some(commit) = reviewed_substrate_key(config) {
        return commit;
    }
    let cargo_root = cargo_cache_root(config);
    let base = cache::rust_hash(cargo_root);
    if cargo_root == config.repo_root.as_path() {
        base
    } else {
        format!(
            "{}-root-{}",
            base,
            cache::cargo_lock_hash(&config.repo_root)
        )
    }
}

/// Cache-key component naming the substrate a cargo check will actually analyse,
/// when that substrate is NOT the local working tree.
///
/// The reviewed tree is fully determined by the target commit, so the commit id
/// IS the content key — no file hashing needed, and no chance of colliding with
/// a local-tree key written by an earlier local run (which would serve the local
/// checkout's verdict for a PR).
///
/// The commit alone is not the whole substrate though: the same commit checked
/// from the workspace root and from a configured member yields different
/// check/clippy/audit/rustfmt results, so the cargo root travels in the key too
/// — the same discriminator the local hash path already carries.
///
/// `None` when the configured cargo root lies outside the repo: then no reviewed
/// tree is analysed at all (the check is skipped, see
/// [`unreachable_reviewed_cargo_root`]) and a commit-shaped key would promise a
/// result about a commit nothing scanned.
fn reviewed_substrate_key(config: &Config) -> Option<String> {
    let commit = off_head_target_commit(config)?;
    reviewed_substrate_key_for(&commit, cargo_cache_root(config), &config.repo_root)
}

/// Pure half of [`reviewed_substrate_key`].
fn reviewed_substrate_key_for(commit: &str, local_root: &Path, repo_root: &Path) -> Option<String> {
    let relative = repo_relative_cargo_root(local_root, repo_root)?;
    Some(format!(
        "commit-{commit}-root-{}",
        cargo_root_token(&relative)
    ))
}

/// The repo-relative cargo root as a git-style path (`crates/core`, empty string
/// for the repo root itself) — the form git trees and cache keys both need.
fn cargo_root_path(relative: &Path) -> String {
    relative
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The cargo root as ONE file-name-safe cache-key component.
///
/// A cache key is a file name (`<cache_dir>/<check>/<key>`), and `Cache::set`
/// creates only the check-level directory. A nested root written verbatim put a
/// separator inside the key, so the write targeted a directory that never
/// existed: every store failed and check/clippy/audit/geiger — the slowest gates
/// in the tool — recomputed on every review of a workspace member. Hashing the
/// path keeps the discriminator exact while staying one component; `self` marks
/// the repo root, and cannot be confused with a hash (hex has no `s`).
fn cargo_root_token(relative: &Path) -> String {
    let path = cargo_root_path(relative);
    if path.is_empty() {
        return "self".to_string();
    }
    cache::key_token(&path)
}

/// Source hash for source-only cargo checks (rustfmt): the reviewed commit when
/// one is being analysed, the local tree hash otherwise.
fn cargo_source_hash(config: &Config) -> String {
    reviewed_substrate_key(config).unwrap_or_else(|| cache::rust_hash(cargo_cache_root(config)))
}

#[async_trait]
impl Check for CargoCheck {
    fn name(&self) -> &str {
        "Cargo check"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if let Some(reason) = unreachable_reviewed_cargo_root(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if let Some(reason) = missing_reviewed_cargo_manifest(config) {
            return super::CheckEligibility::Skip(reason);
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(cargo_content_hash(config))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let run = plan_cargo_run(config)?;
        let cwd = run.cwd.as_path();

        let args = &["check", "--message-format=short"];
        let output = run_command_with_env("cargo", args, cwd, &run.env).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    repo_root: &config.repo_root,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_repo_relative_cwd(),
            ),
        })
    }
}

#[async_trait]
impl Check for ClippyCheck {
    fn name(&self) -> &str {
        "Clippy"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if let Some(reason) = unreachable_reviewed_cargo_root(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if let Some(reason) = missing_reviewed_cargo_manifest(config) {
            return super::CheckEligibility::Skip(reason);
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("clippy-{}", cargo_content_hash(config)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let run = plan_cargo_run(config)?;
        let cwd = run.cwd.as_path();

        let args = &["clippy", "--message-format=short", "--", "-D", "warnings"];
        let output = run_command_with_env("cargo", args, cwd, &run.env).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            if clippy_has_real_warnings(&combined) {
                CheckStatus::Warnings
            } else {
                CheckStatus::Passed
            }
        } else {
            CheckStatus::Failed
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    repo_root: &config.repo_root,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_repo_relative_cwd(),
            ),
        })
    }
}

/// Detect a cargo build-script (`build.rs`) warning print.
///
/// `cargo::warning=` / `cargo:warning=` output from a dependency's build script
/// is surfaced by the compiler driver as a line shaped like
/// `warning: <pkg>@<version>: <message>` (e.g.
/// `warning: codescribe-core@0.12.2: Embedding MiniLM model from: ...`). These
/// are diagnostic prints from a `build.rs`, not rustc/clippy lints, so they must
/// not flip an otherwise-clean clippy run to a WARN status. They are recognised
/// by the `name@version:` prefix that immediately follows the `warning: ` marker
/// — a real lint message never carries an `@version` token before its first
/// `": "` separator.
fn is_cargo_build_script_warning(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("warning: ") else {
        return false;
    };
    match rest.split_once(": ") {
        Some((prefix, _)) => prefix.contains('@') && !prefix.contains(char::is_whitespace),
        None => false,
    }
}

/// True when clippy/rustc emitted at least one real lint warning, ignoring
/// build-script `cargo:warning=` noise emitted by dependencies' `build.rs`.
fn clippy_has_real_warnings(combined: &str) -> bool {
    combined
        .lines()
        .filter(|line| line.contains("warning:"))
        .any(|line| !is_cargo_build_script_warning(line))
}

#[async_trait]
impl Check for CargoTestCheck {
    fn name(&self) -> &str {
        "Cargo test"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.run_tests {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_tests {
            return super::CheckEligibility::Skip("tests disabled".to_string());
        }
        if let Some(reason) = unreachable_reviewed_cargo_root(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if let Some(reason) = missing_reviewed_cargo_manifest(config) {
            return super::CheckEligibility::Skip(reason);
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // Tests shouldn't be cached
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let run = plan_cargo_run(config)?;
        let cwd = run.cwd.as_path();

        let args = &["test", "--all-targets", "--no-fail-fast"];
        let output =
            run_command_with_timeout_and_env("cargo", args, cwd, TEST_TIMEOUT_SECS, &run.env)
                .await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    repo_root: &config.repo_root,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_repo_relative_cwd(),
            ),
        })
    }
}

#[async_trait]
impl Check for RustfmtCheck {
    fn name(&self) -> &str {
        "Rustfmt"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if config.is_fast_remote_only_standard() && !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if !config.should_run_heavy_rust_lint() {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        if let Some(reason) = unreachable_reviewed_cargo_root(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if let Some(reason) = missing_reviewed_cargo_manifest(config) {
            return super::CheckEligibility::Skip(reason);
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("rustfmt-{}", cargo_source_hash(config)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let run = plan_cargo_run(config)?;
        let cwd = run.cwd.as_path();

        let args = &["fmt", "--check"];
        let output = run_command_with_env("cargo", args, cwd, &run.env).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = classify_rustfmt_status(output.status.success(), &combined);
        let result_output = if status == CheckStatus::Skipped {
            format!(
                "Rustfmt skipped: rustfmt component is not installed or cargo fmt is unavailable.\n{combined}"
            )
        } else if status == CheckStatus::Error {
            format!("Rustfmt error: cargo fmt failed unexpectedly.\n{combined}")
        } else {
            combined.clone()
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: result_output.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    repo_root: &config.repo_root,
                    output: &output,
                    combined_output: &result_output,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_repo_relative_cwd(),
            ),
        })
    }
}

fn classify_rustfmt_status(command_succeeded: bool, output: &str) -> CheckStatus {
    if command_succeeded {
        return CheckStatus::Passed;
    }

    if rustfmt_tool_unavailable(output) {
        return CheckStatus::Skipped;
    }

    if has_tool_crash(output) {
        return CheckStatus::Error;
    }

    // `cargo fmt --check` exits non-zero when files need formatting.
    CheckStatus::Warnings
}

fn rustfmt_tool_unavailable(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    (lower.contains("rustfmt") || lower.contains("cargo-fmt"))
        && (lower.contains("is not installed")
            || lower.contains("component") && lower.contains("missing")
            || lower.contains("no such command") && lower.contains("fmt"))
}

#[async_trait]
impl Check for CargoAuditCheck {
    fn name(&self) -> &str {
        "Cargo audit"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if !config.run_security && !config.run_lint {
            return super::CheckEligibility::Skip("security disabled".to_string());
        }
        if which::which("cargo-audit").is_err() {
            return super::CheckEligibility::Skip(
                "tool not installed (cargo-audit is missing)".to_string(),
            );
        }
        if let Some(reason) = unreachable_reviewed_cargo_root(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if let Some(reason) = missing_reviewed_cargo_manifest(config) {
            return super::CheckEligibility::Skip(reason);
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        // Advisories change over time even when the code does not, so the audit
        // result must not be cached indefinitely — a freshly published RUSTSEC
        // advisory has to reach the gate. Key on the dependency manifest plus the
        // current day: repeated runs on the same day stay cached, but a new day
        // (or a Cargo.lock change) re-runs the audit. Source churn is irrelevant
        // to the advisory set, so it is deliberately excluded.
        let day = Local::now().format("%Y-%m-%d");
        // Hash the lock at the SAME directory the audit runs in — cargo_root,
        // which may be a workspace member with its own Cargo.lock — not the repo
        // root. Keying on the root lock while executing in a member meant a
        // member Cargo.lock change never invalidated the cache and a stale audit
        // was served (PR #12 review #22). When a reviewed commit is analysed the
        // lock that matters lives in the snapshot, and the commit id names it
        // exactly.
        let lock = reviewed_substrate_key(config)
            .unwrap_or_else(|| cache::cargo_lock_hash(cargo_cache_root(config)));
        Some(format!("audit-{lock}-{day}"))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let run = plan_cargo_run(config)?;
        let cwd = run.cwd.as_path();

        let args = &["audit", "--json"];
        let output = run_command_with_env("cargo", args, cwd, &run.env).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);
        let status = classify_cargo_audit_status(output.status.success(), &stdout, &combined);

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    repo_root: &config.repo_root,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_repo_relative_cwd(),
            ),
        })
    }
}

fn classify_cargo_audit_status(
    command_succeeded: bool,
    stdout: &str,
    combined: &str,
) -> CheckStatus {
    if let Some(vulnerability_count) = cargo_audit_vulnerability_count(stdout) {
        if vulnerability_count > 0 {
            return CheckStatus::Failed;
        }

        if cargo_audit_has_warnings(stdout, combined) {
            return CheckStatus::Warnings;
        }

        if command_succeeded {
            return CheckStatus::Passed;
        }

        return CheckStatus::Failed;
    }

    if cargo_audit_has_warnings(stdout, combined) {
        return CheckStatus::Warnings;
    }

    if command_succeeded {
        return CheckStatus::Passed;
    }

    if combined.contains("RUSTSEC-") {
        return CheckStatus::Failed;
    }

    CheckStatus::Failed
}

fn cargo_audit_vulnerability_count(stdout: &str) -> Option<usize> {
    let parsed = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    let vulnerabilities = parsed.get("vulnerabilities")?;

    if let Some(count) = vulnerabilities
        .get("count")
        .and_then(|value| value.as_u64())
    {
        return Some(count as usize);
    }

    if let Some(list) = vulnerabilities
        .get("list")
        .and_then(|value| value.as_array())
    {
        return Some(list.len());
    }

    Some(0)
}

fn cargo_audit_has_warnings(stdout: &str, output: &str) -> bool {
    cargo_audit_warning_count(stdout).is_some_and(|count| count > 0)
        || output
            .lines()
            .any(|line| line.to_ascii_lowercase().contains("warning:"))
}

fn cargo_audit_warning_count(stdout: &str) -> Option<usize> {
    let parsed = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    let warnings = parsed.get("warnings")?;
    Some(count_cargo_audit_warning_items(warnings))
}

fn count_cargo_audit_warning_items(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(items) => items.len(),
        serde_json::Value::Object(map) => {
            if let Some(count) = map.get("count").and_then(|value| value.as_u64()) {
                return count as usize;
            }

            map.values().map(count_cargo_audit_warning_items).sum()
        }
        serde_json::Value::Bool(true) => 1,
        _ => 0,
    }
}

#[async_trait]
impl Check for CargoGeigerCheck {
    fn name(&self) -> &str {
        "Cargo geiger"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.has_cargo {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if !config.run_security {
            // An explicit global security-off (`--skip-security`, or a fast
            // preset like `--quick` without `--with-security`) must win over
            // `--security-full`: geiger is minutes-slow and has no business
            // running when security was deliberately disabled. `--security-full`
            // on its own still implies security intent (see
            // `Cli::should_run_security`), so the plain opt-in path stays live.
            return super::CheckEligibility::Skip("security disabled".to_string());
        }
        if !config.security_full {
            return super::CheckEligibility::Skip("requires --security-full".to_string());
        }
        if which::which("cargo-geiger").is_err() {
            return super::CheckEligibility::Skip(
                "tool not installed (cargo-geiger is missing)".to_string(),
            );
        }
        if let Some(reason) = unreachable_reviewed_cargo_root(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if let Some(reason) = missing_reviewed_cargo_manifest(config) {
            return super::CheckEligibility::Skip(reason);
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        Some(format!("geiger-{}", cargo_content_hash(config)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let run = plan_cargo_run(config)?;
        let cwd = run.cwd.as_path();

        if cargo_metadata_is_virtual_manifest(cwd, &run.env).await {
            return Ok(CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Skipped,
                duration: start.elapsed(),
                output: "Cargo geiger skipped: cargo metadata reports a virtual workspace manifest; cargo-geiger requires a concrete package. Configure package selection or run geiger per workspace member.".to_string(),
                cached: false,
                provenance: None,
            });
        }

        let args = &["geiger", "--output-format", "Ratio"];
        let output = match run_command_with_timeout_and_env("cargo", args, cwd, 600, &run.env).await
        {
            Ok(output) => output,
            Err(err) if super::is_timeout_error(&err) => {
                // `cargo geiger` can take many minutes on large dependency trees
                // and is a non-blocking advisory signal. A timeout is a tooling
                // limitation, not a quality failure — degrade to Skipped instead
                // of a hard Error so it does not pollute the merge gate.
                return Ok(CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Skipped,
                    duration: start.elapsed(),
                    output: format!("cargo geiger skipped: {err}"),
                    cached: false,
                    provenance: None,
                });
            }
            Err(err) => return Err(err),
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = classify_cargo_geiger_status(output.status.success(), &combined);

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "cargo",
                    args,
                    cwd,
                    repo_root: &config.repo_root,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: self.cache_key(config),
                }
                .build_repo_relative_cwd(),
            ),
        })
    }
}

fn classify_cargo_geiger_status(command_succeeded: bool, output: &str) -> CheckStatus {
    if output.contains("is a virtual manifest")
        && output.contains("requires running against an actual package")
    {
        return CheckStatus::Skipped;
    }

    if has_tool_crash(output) {
        return CheckStatus::Error;
    }

    // A virtual workspace manifest (the root `Cargo.toml` of a `[workspace]`
    // with no `[package]`) cannot be scanned by `cargo geiger` directly — cargo
    // refuses with "requires running against an actual package". That is a
    // workspace-shape limitation, not an unsafe-code signal or a tool crash, so
    // degrade to a clean Skipped status instead of a permanent gate error.
    if is_virtual_manifest_error(output) {
        return CheckStatus::Skipped;
    }

    let dependency_scan_warnings = output
        .lines()
        .any(|line| line.starts_with("WARNING: Dependency file was never scanned:"));
    let warning_summary = output.lines().any(|line| {
        line.starts_with("error: Found ")
            && line
                .split_whitespace()
                .last()
                .is_some_and(|token| token == "warnings")
    });

    if !command_succeeded {
        if dependency_scan_warnings && warning_summary {
            return CheckStatus::Warnings;
        }
        return CheckStatus::Error;
    }

    // Command succeeded: read the actual `used/total=pct%` ratio table.
    //
    // The old `contains("0/0") || !contains("unsafe")` heuristic was structurally
    // blind: geiger's legend ALWAYS contains the word "unsafe" (dead second
    // branch), and a `0/0=100.00%` cell appears in the Impls/Traits/Methods
    // columns of nearly every crate — including crates that DO use unsafe — so
    // the first branch painted real unsafe green. Classify from the numbers.
    match geiger_unsafe_found(output) {
        Some(true) => CheckStatus::Warnings,
        Some(false) => CheckStatus::Passed,
        // Exit 0 but no ratio table parsed — an unexpected output shape we must
        // not report as a clean Passed (fail-open on a security signal).
        None => CheckStatus::Warnings,
    }
}

/// Read `cargo geiger --output-format Ratio` output for unsafe usage.
///
/// Each ratio cell is `used/total=pct%` where `used` counts SAFE code out of
/// `total`; a cell with `total > used` means unsafe items were found. Legend
/// lines are prose (their literal `x/y=z%` is non-numeric) and are skipped.
///
/// Returns `Some(true)` if any cell reports unsafe, `Some(false)` if a table was
/// found and every cell is clean, and `None` if no ratio cell was parsed at all.
fn geiger_unsafe_found(output: &str) -> Option<bool> {
    let mut saw_row = false;
    let mut unsafe_found = false;
    for token in output.split_whitespace() {
        let Some((ratio, _pct)) = token.split_once('=') else {
            continue;
        };
        let Some((safe, total)) = ratio.split_once('/') else {
            continue;
        };
        let (Ok(safe), Ok(total)) = (safe.parse::<u64>(), total.parse::<u64>()) else {
            continue;
        };
        saw_row = true;
        if total > safe {
            unsafe_found = true;
        }
    }
    saw_row.then_some(unsafe_found)
}

/// Detect cargo's "virtual manifest" refusal, emitted when `cargo geiger` is
/// run at the root of a `[workspace]` whose manifest declares no package.
fn is_virtual_manifest_error(output: &str) -> bool {
    output.contains("virtual manifest")
        && output.contains("requires running against an actual package")
}

async fn cargo_metadata_is_virtual_manifest(cwd: &Path, env: &[(String, String)]) -> bool {
    // Async with a hard timeout: a synchronous `cargo metadata` here blocked
    // the whole FuturesUnordered check pool whenever cargo sat on a file lock
    // (e.g. a parallel build in the same repo) — the in-process cousin of the
    // npx hang class.
    let Ok(output) = crate::checks::run_command_with_timeout_and_env(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1"],
        cwd,
        60,
        env,
    )
    .await
    else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    let Ok(metadata) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return false;
    };

    metadata.get("root_package").is_some_and(|v| v.is_null())
        && metadata
            .get("workspace_members")
            .and_then(|v| v.as_array())
            .is_some_and(|members| !members.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ExecutionMode;
    use crate::config::{test_config_builder, test_rust_profile};

    fn create_test_config(has_cargo: bool, run_lint: bool, run_tests: bool) -> Config {
        test_config_builder()
            .profile(test_rust_profile(has_cargo))
            .execution_mode(ExecutionMode::Standard)
            .run_lint(run_lint)
            .run_tests(run_tests)
            .do_fetch(false)
            .use_cache(false)
            .create_zip(false)
            .build()
    }

    #[test]
    fn test_cargo_check_name() {
        let check = CargoCheck;
        assert_eq!(check.name(), "Cargo check");
    }

    #[test]
    fn test_clippy_check_name() {
        let check = ClippyCheck;
        assert_eq!(check.name(), "Clippy");
    }

    #[test]
    fn test_cargo_check_can_run_with_cargo() {
        let config = create_test_config(true, false, false);
        let check = CargoCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_cargo_check_cannot_run_without_cargo() {
        let config = create_test_config(false, false, false);
        let check = CargoCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_can_run() {
        let config = create_test_config(true, true, false);
        let check = ClippyCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_clippy_check_cannot_run_without_lint() {
        let config = create_test_config(true, false, false);
        let check = ClippyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_cannot_run_without_cargo() {
        let config = create_test_config(false, true, false);
        let check = ClippyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_skips_fast_remote_only_by_default() {
        let mut config = create_test_config(true, true, false);
        config.remote_only = true;
        let check = ClippyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_clippy_check_can_run_fast_remote_only_when_forced() {
        let mut config = create_test_config(true, true, false);
        config.remote_only = true;
        config.lint_forced = true;
        let check = ClippyCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_cargo_test_check_can_run() {
        let config = create_test_config(true, false, true);
        let check = CargoTestCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_cargo_test_check_cannot_run_without_tests() {
        let config = create_test_config(true, false, false);
        let check = CargoTestCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_cargo_geiger_requires_security_full() {
        // Without --security-full geiger is not eligible; it is opt-in and must
        // stay out of the default profile rather than fabricate a caveat.
        let mut config = create_test_config(true, true, false);
        config.run_security = true;
        assert!(!config.security_full);
        let check = CargoGeigerCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_cargo_geiger_skipped_when_security_disabled() {
        // Even with --security-full, an explicit global security-off
        // (`--skip-security`, or a fast preset without `--with-security`) leaves
        // `run_security = false`. The minutes-slow geiger scan must not run in
        // that state, and the skip reason must name the disable, not the flag.
        let mut config = create_test_config(true, true, false);
        config.run_security = false;
        config.security_full = true;
        let check = CargoGeigerCheck;
        match check.check_eligibility(&config) {
            super::super::CheckEligibility::Skip(reason) => {
                assert!(
                    reason.contains("security disabled"),
                    "unexpected skip reason: {reason}"
                );
            }
            super::super::CheckEligibility::Run => {
                panic!("geiger must not run when security is globally disabled")
            }
        }
    }

    #[test]
    fn test_cargo_check_cache_key() {
        let config = create_test_config(true, false, false);
        let check = CargoCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
    }

    #[test]
    fn test_clippy_check_cache_key() {
        let config = create_test_config(true, true, false);
        let check = ClippyCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
        assert!(key.unwrap().starts_with("clippy-"));
    }

    fn config_with_cargo_root(repo_root: &Path, cargo_root: &Path, run_lint: bool) -> Config {
        let mut profile = test_rust_profile(true);
        profile.cargo_root = Some(cargo_root.to_path_buf());
        test_config_builder()
            .repo_root(repo_root)
            .profile(profile)
            .run_lint(run_lint)
            .build()
    }

    fn assert_cache_key_changes_after_member_lock_bump(check: &dyn Check, run_lint: bool) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let member = root.join("member");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "# root lock\n").unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        std::fs::write(member.join("Cargo.lock"), "# member lock v1\n").unwrap();
        std::fs::write(member.join("src/lib.rs"), "pub fn demo() {}\n").unwrap();

        let config = config_with_cargo_root(root, &member, run_lint);
        let before = check.cache_key(&config).expect("cache key before");
        std::fs::write(member.join("Cargo.lock"), "# member lock v2\n").unwrap();
        let after = check.cache_key(&config).expect("cache key after");
        assert_ne!(
            before,
            after,
            "{} cache key must hash the configured cargo_root manifest set",
            check.name()
        );
    }

    /// A workspace member with no lockfile of its own must still invalidate its
    /// cache key when the workspace-root `Cargo.lock` is bumped — Cargo resolves
    /// deps from the root lock, so a root-only dependency change alters what is
    /// compiled even though no member file moved.
    fn assert_cache_key_changes_after_root_lock_bump(check: &dyn Check, run_lint: bool) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let member = root.join("member");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "# root lock v1\n").unwrap();
        // Member has NO Cargo.lock of its own (the realistic workspace shape).
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        std::fs::write(member.join("src/lib.rs"), "pub fn demo() {}\n").unwrap();

        let config = config_with_cargo_root(root, &member, run_lint);
        let before = check.cache_key(&config).expect("cache key before");
        // Bump ONLY the workspace-root lockfile — no member file changes.
        std::fs::write(root.join("Cargo.lock"), "# root lock v2 bumped dep\n").unwrap();
        let after = check.cache_key(&config).expect("cache key after");
        assert_ne!(
            before,
            after,
            "{} cache key must fold the workspace-root lockfile for member cargo roots",
            check.name()
        );
    }

    #[test]
    fn test_cargo_check_cache_key_reflects_workspace_root_lock_bump() {
        assert_cache_key_changes_after_root_lock_bump(&CargoCheck, false);
    }

    #[test]
    fn test_clippy_cache_key_reflects_workspace_root_lock_bump() {
        assert_cache_key_changes_after_root_lock_bump(&ClippyCheck, true);
    }

    #[test]
    fn test_geiger_cache_key_reflects_workspace_root_lock_bump() {
        assert_cache_key_changes_after_root_lock_bump(&CargoGeigerCheck, false);
    }

    #[test]
    fn test_cargo_check_cache_key_follows_cargo_root_not_repo_root() {
        assert_cache_key_changes_after_member_lock_bump(&CargoCheck, false);
    }

    #[test]
    fn test_clippy_check_cache_key_follows_cargo_root_not_repo_root() {
        assert_cache_key_changes_after_member_lock_bump(&ClippyCheck, true);
    }

    #[test]
    fn test_rustfmt_cache_key_follows_cargo_root_not_repo_root() {
        assert_cache_key_changes_after_member_lock_bump(&RustfmtCheck, true);
    }

    #[test]
    fn test_cargo_geiger_cache_key_follows_cargo_root_not_repo_root() {
        assert_cache_key_changes_after_member_lock_bump(&CargoGeigerCheck, true);
    }

    #[test]
    fn test_rustfmt_missing_component_is_skipped_not_warnings() {
        let output =
            "error: 'rustfmt' is not installed for the toolchain 'stable-aarch64-apple-darwin'\n";
        assert_eq!(classify_rustfmt_status(false, output), CheckStatus::Skipped);
    }

    #[test]
    fn test_cargo_audit_cache_key_is_day_scoped() {
        // The audit key must carry the current day so a freshly published
        // advisory invalidates a cached "passed" within a day, rather than being
        // pinned forever to an unchanged Cargo.lock.
        let config = create_test_config(true, true, false);
        let check = CargoAuditCheck;
        let key = check.cache_key(&config).expect("audit cache key");
        assert!(key.starts_with("audit-"), "unexpected key: {key}");
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert!(
            key.ends_with(&today),
            "audit key must be scoped to the current day ({today}), got: {key}"
        );
    }

    #[test]
    fn test_cargo_audit_cache_key_follows_cargo_root_not_repo_root() {
        // PR #12 review #22: the audit runs in cargo_root (which may be a
        // workspace member), so the cache key must hash THAT directory's
        // Cargo.lock. Keying on the repo root while executing in a member let a
        // member lock change go unnoticed and served a stale audit. Two configs
        // that differ ONLY in cargo_root (root vs member, with different locks)
        // must therefore produce different keys.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let member = root.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "# root lock\n").unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        std::fs::write(member.join("Cargo.lock"), "# member lock DIFFERENT\n").unwrap();

        let mut root_profile = test_rust_profile(true);
        root_profile.cargo_root = Some(root.to_path_buf());
        let config_root = test_config_builder()
            .repo_root(root)
            .profile(root_profile)
            .build();

        let mut member_profile = test_rust_profile(true);
        member_profile.cargo_root = Some(member.clone());
        let config_member = test_config_builder()
            .repo_root(root)
            .profile(member_profile)
            .build();

        let check = CargoAuditCheck;
        let key_root = check.cache_key(&config_root).expect("root key");
        let key_member = check.cache_key(&config_member).expect("member key");
        assert_ne!(
            key_root, key_member,
            "audit key must follow cargo_root, not the shared repo root"
        );
    }

    #[test]
    fn test_cargo_audit_vulnerabilities_are_failed() {
        let stdout = r#"{
  "vulnerabilities": {
    "found": true,
    "count": 2,
    "list": [
      {"advisory": {"id": "RUSTSEC-2023-0001"}},
      {"advisory": {"id": "RUSTSEC-2023-0002"}}
    ]
  }
}"#;

        let status = classify_cargo_audit_status(false, stdout, stdout);
        assert_eq!(status, CheckStatus::Failed);
    }

    #[test]
    fn test_cargo_audit_clean_report_is_passed() {
        let stdout = r#"{
  "vulnerabilities": {
    "found": false,
    "count": 0,
    "list": []
  }
}"#;

        let status = classify_cargo_audit_status(true, stdout, stdout);
        assert_eq!(status, CheckStatus::Passed);
    }

    #[test]
    fn test_cargo_audit_warning_only_is_warnings() {
        let stdout = r#"{
  "vulnerabilities": {
    "found": false,
    "count": 0,
    "list": []
  }
}"#;
        let stderr = "warning: advisory database is stale";
        let combined = format!("{}\n{}", stdout, stderr);

        let status = classify_cargo_audit_status(false, stdout, &combined);
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn test_cargo_audit_informational_warning_exit_zero_is_warnings() {
        let stdout = r#"{
  "vulnerabilities": {
    "found": false,
    "count": 0,
    "list": []
  },
  "warnings": {
    "unmaintained": [
      {"advisory": {"id": "RUSTSEC-2024-0001"}}
    ],
    "yanked": [],
    "notice": []
  }
}"#;

        let status = classify_cargo_audit_status(true, stdout, stdout);
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn test_cargo_audit_non_json_failure_is_failed() {
        let combined = "error: failed to fetch advisory db";
        let status = classify_cargo_audit_status(false, "not-json", combined);
        assert_eq!(status, CheckStatus::Failed);
    }

    #[test]
    fn test_cargo_geiger_warning_flood_is_non_blocking_warning() {
        let output = "\
WARNING: Dependency file was never scanned: /tmp/dep.rs
WARNING: Dependency file was never scanned: /tmp/dep2.rs
error: Found 2 warnings
";

        let status = classify_cargo_geiger_status(false, output);
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn test_cargo_geiger_real_failures_stay_errors() {
        let output = "error: cargo-geiger panicked";
        let status = classify_cargo_geiger_status(false, output);
        assert_eq!(status, CheckStatus::Error);
    }

    #[test]
    fn test_cargo_geiger_virtual_manifest_degrades_to_skipped() {
        let output = "manifest path `/repo/Cargo.toml` is a virtual manifest, \
            but this command requires running against an actual package in this workspace";
        let status = classify_cargo_geiger_status(false, output);
        assert_eq!(status, CheckStatus::Skipped);
    }

    // Real `cargo geiger 0.13.0 --output-format Ratio` output for a crate that
    // uses unsafe (`!` marker, one Expressions cell at 1/2). Note it contains
    // BOTH "0/0" and the word "unsafe" (legend) — the exact input the old
    // `contains("0/0") || !contains("unsafe")` heuristic mis-classified as
    // Passed. The blind security signal must now surface as Warnings.
    const GEIGER_RATIO_WITH_UNSAFE: &str = "\
Metric output format: x/y=z%
    x = safe code found in the crate
    y = total code found in the crate
    z = percentage of safe ratio as defined by x/y

Symbols:
    :) = No `unsafe` usage found, declares #![forbid(unsafe_code)]
    ?  = No `unsafe` usage found, missing #![forbid(unsafe_code)]
    !  = `unsafe` usage found

Functions  Expressions  Impls  Traits  Methods  Dependency

    2/2=100.00%     1/2=50.00%         0/0=100.00%        0/0=100.00%     0/0=100.00%  !  geigertest 0.1.0

    2/2=100.00%     1/2=50.00%         0/0=100.00%        0/0=100.00%     0/0=100.00%
";

    // Same shape, but every category is fully safe (`?` marker, all n/n). Still
    // contains "0/0" and the legend word "unsafe" — must classify as Passed
    // without the legend tripping a false Warning.
    const GEIGER_RATIO_ALL_SAFE: &str = "\
Symbols:
    :) = No `unsafe` usage found, declares #![forbid(unsafe_code)]
    ?  = No `unsafe` usage found, missing #![forbid(unsafe_code)]
    !  = `unsafe` usage found

Functions  Expressions  Impls  Traits  Methods  Dependency

    5/5=100.00%     3/3=100.00%     0/0=100.00%     0/0=100.00%     1/1=100.00%  ?  safecrate 0.1.0

    5/5=100.00%     3/3=100.00%     0/0=100.00%     0/0=100.00%     1/1=100.00%
";

    #[test]
    fn cargo_geiger_unsafe_ratio_is_warnings_not_passed() {
        // Regression: blind != healthy. A crate that uses unsafe must never be
        // painted green just because "0/0" and "unsafe" appear in the output.
        let status = classify_cargo_geiger_status(true, GEIGER_RATIO_WITH_UNSAFE);
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn cargo_geiger_all_safe_ratio_is_passed() {
        let status = classify_cargo_geiger_status(true, GEIGER_RATIO_ALL_SAFE);
        assert_eq!(status, CheckStatus::Passed);
    }

    #[test]
    fn cargo_geiger_success_without_ratio_table_is_not_green() {
        // Exit 0 but no ratio table at all — an unexpected shape that the old
        // heuristic would have painted Passed (no "unsafe", no "0/0"). Honesty
        // gate: do not fake a clean security result.
        let status = classify_cargo_geiger_status(true, "some unexpected geiger output\n");
        assert_eq!(status, CheckStatus::Warnings);
    }

    #[test]
    fn geiger_unsafe_found_reads_ratio_cells() {
        assert_eq!(geiger_unsafe_found(GEIGER_RATIO_WITH_UNSAFE), Some(true));
        assert_eq!(geiger_unsafe_found(GEIGER_RATIO_ALL_SAFE), Some(false));
        assert_eq!(geiger_unsafe_found("no ratios here"), None);
        // 0/0 must not count as unsafe (no code in that category).
        assert_eq!(geiger_unsafe_found("0/0=100.00%"), Some(false));
    }

    #[test]
    fn clippy_build_script_warnings_do_not_trip_warn_status() {
        // Real-world clippy log from a clean run where a dependency's build.rs
        // prints `cargo:warning=` lines. Clippy itself is clean (exit 0, no
        // lints), so the check must report Passed, not Warnings.
        let combined = "\n\
warning: codescribe-core@0.12.2: Embedding MiniLM model from: /Users/x/models\n\
warning: codescribe-core@0.12.2: Embedded models for codescribe: Whisper=runtime_load_from_cache\n   \
Compiling codescribe v0.12.2 (/repo)\n    \
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.46s\n";
        assert!(!clippy_has_real_warnings(combined));
    }

    #[test]
    fn clippy_real_lint_warning_trips_warn_status() {
        let combined =
            "src/main.rs:10:5: warning: unused variable `x`\nwarning: 1 warning emitted\n";
        assert!(clippy_has_real_warnings(combined));
    }

    #[test]
    fn clippy_mixed_build_script_and_real_warning_is_real() {
        let combined = "\
warning: dep-crate@1.2.3: building native lib\n\
src/lib.rs:3:1: warning: function `foo` is never used\n";
        assert!(clippy_has_real_warnings(combined));
    }

    #[test]
    fn is_cargo_build_script_warning_detection() {
        assert!(is_cargo_build_script_warning(
            "warning: codescribe-core@0.12.2: Embedding MiniLM model from: /x"
        ));
        assert!(is_cargo_build_script_warning(
            "warning: some-crate@1.0.0-beta.1: doing native build work"
        ));
        // Real clippy lints are never build-script warnings.
        assert!(!is_cargo_build_script_warning(
            "src/main.rs:10:5: warning: unused variable `x`"
        ));
        assert!(!is_cargo_build_script_warning(
            "warning: unused import: `std::io`"
        ));
        assert!(!is_cargo_build_script_warning(
            "warning: 2 warnings emitted"
        ));
    }

    #[test]
    fn test_is_virtual_manifest_error_detection() {
        assert!(is_virtual_manifest_error(
            "Cargo.toml is a virtual manifest, but this command requires running against an actual package",
        ));
        assert!(!is_virtual_manifest_error("error: Found 3 warnings"));
        assert!(!is_virtual_manifest_error("a virtual manifest"));
    }

    /// Write a minimal, dependency-free binary crate whose `main.rs` is either
    /// rustfmt-clean or deliberately mangled.
    fn write_crate(dir: &Path, formatted: bool) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"substrate-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"substrate-fixture\"\npath = \"src/main.rs\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let main = if formatted {
            "fn main() {\n    println!(\"reviewed\");\n}\n"
        } else {
            "fn main(){let x=1;println!(\"stale{}\",x);}\n"
        };
        std::fs::write(dir.join("src/main.rs"), main).unwrap();
    }

    /// Regression: a cargo check must judge the REVIEWED snapshot, never the
    /// local checkout.
    ///
    /// With a `--pr`/`--remote` target, `repo_root` still holds whatever branch
    /// is checked out locally, so the pre-fix code ran build/clippy/test/fmt
    /// against a foreign tree and printed the result under the reviewed PR's
    /// name. The fixture makes the two directories disagree on purpose:
    /// `repo_root` is badly formatted and the scan dir is clean, so running in
    /// the wrong place does not merely show up in provenance — it flips the
    /// verdict.
    #[tokio::test]
    async fn test_cargo_check_runs_in_scan_dir_not_repo_root() {
        if which::which("cargo").is_err() {
            return;
        }

        // repo_root == the stale local checkout: `cargo fmt --check` complains.
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        write_crate(repo_root.path(), false);

        // scan_dir == the reviewed target snapshot: formatting is clean.
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
        write_crate(scan_dir.path(), true);

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(repo_root.path().to_path_buf());
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let result = RustfmtCheck.run(&config).await.expect("rustfmt run");
        if result.status == CheckStatus::Skipped {
            // rustfmt component missing — nothing to assert about the substrate.
            return;
        }

        assert_eq!(
            result.status,
            CheckStatus::Passed,
            "the cargo check must judge the reviewed snapshot's clean tree, not \
             repo_root's mangled one. Output: {}",
            result.output
        );

        // Provenance must name the directory the run actually used.
        let cwd = result.provenance.expect("provenance").cwd;
        let scan_dir_name = scan_dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            cwd.contains(&scan_dir_name),
            "provenance cwd must report the reviewed scan dir, got {cwd}",
        );
    }

    /// The reviewed snapshot is a throwaway temp dir, so its in-tree `target/`
    /// would force a full dependency rebuild on every run. `CARGO_TARGET_DIR`
    /// must therefore point at the per-repo shared build cache — outside both
    /// the snapshot and the operator's own tree.
    #[tokio::test]
    async fn test_cargo_run_uses_shared_build_cache_off_head() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(repo_root.path().to_path_buf());
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let run = plan_cargo_run(&config).expect("plan");

        assert_eq!(run.cwd, scan_dir.path());
        assert_eq!(
            run.env,
            vec![(
                "CARGO_TARGET_DIR".to_string(),
                config.cargo_build_cache_dir().display().to_string()
            )],
            "cargo must build into the shared per-repo cache, not the snapshot",
        );
        let target_dir = config.cargo_build_cache_dir();
        assert!(
            !target_dir.starts_with(scan_dir.path()),
            "a build cache inside the throwaway snapshot caches nothing",
        );
        assert!(
            !target_dir.starts_with(repo_root.path()),
            "prview must not write into the operator's own tree",
        );
    }

    /// A local review (target == HEAD) is unchanged: the local cargo root, and
    /// no `CARGO_TARGET_DIR` redirect, so the operator's warm `target/` is used
    /// exactly as before.
    #[tokio::test]
    async fn test_cargo_run_local_target_is_unchanged() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(repo_root.path().to_path_buf());
        // No scan_dir_override, and a non-git repo_root: plan_check_run resolves
        // back to the working tree — the ordinary local path.

        let run = plan_cargo_run(&config).expect("plan");

        assert_eq!(run.cwd, repo_root.path());
        assert!(
            run.env.is_empty(),
            "a local run must not redirect the build directory",
        );
    }

    /// Commit everything in `root` under a fresh test identity.
    fn commit_all(root: &Path, message: &str) {
        use crate::git::cmd::git_cmd;

        for args in [
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", message, "--no-verify"],
        ] {
            let out = git_cmd()
                .args(&args)
                .current_dir(root)
                .output()
                .expect("git command");
            assert!(out.status.success(), "git {args:?} failed");
        }
    }

    fn repo_with_two_commits() -> (tempfile::TempDir, String) {
        repo_with_two_commits_containing(&[])
    }

    /// Two commits in a fresh repo, both carrying `manifests` (repo-relative
    /// paths, written as minimal `Cargo.toml`s); returns the temp dir and the
    /// FIRST commit, so a config targeting it is off-HEAD.
    fn repo_with_two_commits_containing(manifests: &[&str]) -> (tempfile::TempDir, String) {
        use crate::git::cmd::git_cmd;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let run_git = |args: &[&str]| {
            let out = git_cmd()
                .args(args)
                .current_dir(root)
                .output()
                .expect("git command");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.email", "prview@example.test"]);
        run_git(&["config", "user.name", "prview test"]);
        run_git(&["config", "commit.gpgsign", "false"]);
        for manifest in manifests {
            let path = root.join(manifest);
            std::fs::create_dir_all(path.parent().expect("manifest parent")).unwrap();
            std::fs::write(path, "[package]\nname=\"x\"\nversion=\"0.0.0\"\n").unwrap();
        }
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "one"]);
        let first = String::from_utf8(
            git_cmd()
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        run_git(&["commit", "-qam", "two"]);
        (tmp, first)
    }

    /// A cargo root OUTSIDE the repository cannot be materialised from a
    /// snapshot of that repository. The pre-fix code silently ran cargo there
    /// anyway — scanning the operator's unrelated checkout and filing the result
    /// under the reviewed commit. No verdict beats a foreign tree's verdict.
    #[test]
    fn cargo_checks_skip_when_cargo_root_is_outside_the_repo() {
        let (repo, first) = repo_with_two_commits();
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::fs::write(outside.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();

        let mut profile = test_rust_profile(true);
        profile.cargo_root = Some(outside.path().to_path_buf());
        let config = test_config_builder()
            .repo_root(repo.path())
            .profile(profile)
            .target(Some(&first))
            .run_lint(true)
            .run_tests(true)
            .build();

        assert!(
            unreachable_reviewed_cargo_root(&config).is_some(),
            "an external cargo root cannot host the reviewed commit",
        );
        for check in [
            &CargoCheck as &dyn Check,
            &ClippyCheck,
            &RustfmtCheck,
            &CargoTestCheck,
        ] {
            match check.check_eligibility(&config) {
                super::super::CheckEligibility::Skip(reason) => assert!(
                    reason.contains("outside the repo"),
                    "{} skip reason must name the unreachable root, got: {reason}",
                    check.name()
                ),
                super::super::CheckEligibility::Run => panic!(
                    "{} must not run against a tree the review does not describe",
                    check.name()
                ),
            }
        }
    }

    /// An in-repo cargo root keeps running off-HEAD — the skip above must be
    /// narrow, not a blanket disable of cargo checks in `--pr` mode.
    #[test]
    fn cargo_checks_still_run_off_head_for_an_in_repo_cargo_root() {
        let (repo, first) = repo_with_two_commits_containing(&["crates/core/Cargo.toml"]);
        let mut profile = test_rust_profile(true);
        profile.cargo_root = Some(repo.path().join("crates/core"));
        let config = test_config_builder()
            .repo_root(repo.path())
            .profile(profile)
            .target(Some(&first))
            .build();

        assert!(unreachable_reviewed_cargo_root(&config).is_none());
        assert!(missing_reviewed_cargo_manifest(&config).is_none());
        assert_eq!(
            CargoCheck.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    /// The local checkout is Rust, the reviewed commit is not: the branch under
    /// review dropped its last `Cargo.toml`. Eligibility read the LOCAL profile,
    /// so every cargo gate ran anyway and reported cargo's "could not find
    /// Cargo.toml" as the reviewed commit's verdict — a manufactured failure
    /// against a target that is simply not a cargo project.
    #[test]
    fn cargo_checks_skip_when_the_reviewed_commit_has_no_manifest() {
        // First commit: no manifest anywhere. Second (HEAD): the local Rust tree.
        let (repo, first) = repo_with_two_commits_containing(&[]);
        std::fs::create_dir_all(repo.path().join("crates/core")).unwrap();
        std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        commit_all(repo.path(), "add cargo");

        let config = test_config_builder()
            .repo_root(repo.path())
            .profile(test_rust_profile(true))
            .target(Some(&first))
            .run_lint(true)
            .run_tests(true)
            .build();

        for check in [
            &CargoCheck as &dyn Check,
            &ClippyCheck,
            &RustfmtCheck,
            &CargoTestCheck,
        ] {
            match check.check_eligibility(&config) {
                super::super::CheckEligibility::Skip(reason) => assert!(
                    reason.contains("no Cargo.toml"),
                    "{} skip reason must name the missing manifest, got: {reason}",
                    check.name()
                ),
                super::super::CheckEligibility::Run => panic!(
                    "{} must not report a missing manifest as the reviewed commit's verdict",
                    check.name()
                ),
            }
        }
    }

    /// The reviewed commit may have moved the crate somewhere else entirely
    /// (root workspace pushed into `backend/`). Neither the mapped path nor the
    /// snapshot root has a manifest then, so the run had nowhere to go: the
    /// commit IS a cargo project, and reporting "not a cargo project" is as
    /// false as the missing-manifest failure that reporting replaced.
    #[test]
    fn a_cargo_root_moved_to_another_directory_is_rediscovered() {
        // Target commit: the only manifest lives in `backend/`.
        let (repo, target) = repo_with_two_commits_containing(&["backend/Cargo.toml"]);
        // HEAD (the local checkout): the crate is back at the repo root.
        std::fs::remove_file(repo.path().join("backend/Cargo.toml")).unwrap();
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.0.0\"\n",
        )
        .unwrap();
        commit_all(repo.path(), "move the crate back to the root");

        let config = test_config_builder()
            .repo_root(repo.path())
            .profile(test_rust_profile(true))
            .target(Some(&target))
            .build();

        assert!(
            missing_reviewed_cargo_manifest(&config).is_none(),
            "the reviewed commit is a cargo project — just not where it used to be",
        );
        assert_eq!(
            resolve_reviewed_cargo_root(&config),
            ReviewedCargoRoot::Resolved("backend".to_string()),
            "the run must go where the reviewed manifest actually is",
        );
        assert_eq!(
            CargoCheck.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    /// Discovery only helps when it has ONE answer. A tree with several
    /// candidate roots and no manifest where the run was aimed cannot be
    /// resolved by guessing which crate the review is about.
    #[test]
    fn an_ambiguous_moved_cargo_root_is_not_guessed() {
        let (repo, target) =
            repo_with_two_commits_containing(&["backend/Cargo.toml", "frontend/Cargo.toml"]);
        let config = test_config_builder()
            .repo_root(repo.path())
            .profile(test_rust_profile(true))
            .target(Some(&target))
            .build();

        let reason = missing_reviewed_cargo_manifest(&config).expect("ambiguity must not run");
        assert!(
            reason.contains("backend") && reason.contains("frontend"),
            "the skip reason must name what it could not choose between: {reason}",
        );
    }

    /// The reviewed commit can turn an in-repo cargo root into a symlink
    /// pointing outside the repository. The lexical component check accepts it
    /// (no `..` in the path), and following it runs cargo on the operator's
    /// unrelated tree while the verdict is cached under the reviewed commit —
    /// the external-root hole, reopened by a target-controlled symlink.
    #[tokio::test]
    async fn a_symlinked_cargo_root_never_escapes_the_snapshot() {
        let outside = tempfile::tempdir().expect("outside tempdir");
        write_crate(outside.path(), true);
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
        write_crate(scan_dir.path(), true);
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), scan_dir.path().join("backend")).unwrap();
        #[cfg(not(unix))]
        return;

        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        write_crate(&repo_root.path().join("backend"), true);

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(repo_root.path().join("backend"));
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let cwd = match plan_cargo_run(&config) {
            Ok(run) => run.cwd,
            // Refusing outright is equally honest — what must never happen is a
            // verdict earned outside the reviewed tree.
            Err(err) => {
                assert!(
                    err.to_string().contains("outside"),
                    "unexpected error: {err}"
                );
                return;
            }
        };
        let inside = cwd
            .canonicalize()
            .expect("cwd")
            .starts_with(scan_dir.path().canonicalize().expect("scan_dir"));
        assert!(
            inside,
            "cargo must not follow a symlink out of the reviewed tree, got {}",
            cwd.display(),
        );
    }

    /// A member root that the reviewed commit does not have still counts as a
    /// cargo project when the commit carries a workspace manifest at its root:
    /// that is exactly the fallback `reviewed_cargo_root` performs.
    #[test]
    fn a_moved_cargo_root_is_still_a_cargo_project() {
        let (repo, first) = repo_with_two_commits_containing(&["Cargo.toml"]);
        let mut profile = test_rust_profile(true);
        profile.cargo_root = Some(repo.path().join("backend"));
        let config = test_config_builder()
            .repo_root(repo.path())
            .profile(profile)
            .target(Some(&first))
            .build();

        assert!(
            missing_reviewed_cargo_manifest(&config).is_none(),
            "the snapshot root carries a manifest, so the run has somewhere to go",
        );
    }

    /// The same commit checked from the workspace root and from a configured
    /// member produces different results, so the cargo root must travel in the
    /// cache key beside the commit. Keying on the commit alone let a later run
    /// serve the other root's verdict.
    #[test]
    fn reviewed_substrate_key_discriminates_the_cargo_root() {
        let repo_root = Path::new("/repo");
        let commit = "abc123";

        let root_key = reviewed_substrate_key_for(commit, repo_root, repo_root).expect("root key");
        let member_key =
            reviewed_substrate_key_for(commit, Path::new("/repo/crates/core"), repo_root)
                .expect("member key");

        assert_ne!(
            root_key, member_key,
            "workspace root and member must not share one reviewed-substrate key",
        );
        assert!(root_key.contains(commit) && member_key.contains(commit));
        assert_eq!(
            member_key,
            format!("commit-{commit}-root-{}", cache::key_token("crates/core")),
            "the member key must name exactly its own root",
        );
        assert!(
            root_key.ends_with("-root-self"),
            "the repo root must be named, not hashed away: {root_key}",
        );
        // Two different roots must never collide onto one key.
        assert_ne!(
            reviewed_substrate_key_for(commit, Path::new("/repo/crates/core"), repo_root),
            reviewed_substrate_key_for(commit, Path::new("/repo/crates/cli"), repo_root),
        );
        // A relative `.` root is the repo root itself — same substrate, same key.
        assert_eq!(
            reviewed_substrate_key_for(commit, Path::new("."), repo_root),
            Some(root_key),
        );
    }

    /// A cache key is a FILE NAME. `crates/core` used to travel into it
    /// verbatim, so the key named a file inside a directory `Cache::set` never
    /// creates: every write failed silently and the most expensive checks in the
    /// tool recomputed on every single review of a workspace member.
    #[test]
    fn reviewed_substrate_key_is_usable_as_a_file_name() {
        let key = reviewed_substrate_key_for(
            "abc123",
            Path::new("/repo/crates/core"),
            Path::new("/repo"),
        )
        .expect("member key");
        assert!(
            !key.contains('/') && !key.contains('\\') && !key.contains(':'),
            "a cache key must be a single path component, got: {key}",
        );

        // End to end: the key a nested root produces must survive a real cache
        // round-trip, which is what the embedded separator broke.
        let dir = tempfile::tempdir().expect("cache tempdir");
        let cache = crate::cache::Cache::with_dir(dir.path().to_path_buf(), true);
        cache
            .set("Clippy", &key, "passed", Some("out"), None)
            .expect("a nested-root key must be storable");
        assert_eq!(
            cache.get("Clippy", &key).expect("cache hit").status,
            "passed",
        );
    }

    /// The local (non-reviewed) member key carried the same colon, which is an
    /// illegal file-name character on Windows. Both key paths are file names and
    /// both must stay portable.
    #[test]
    fn local_member_cache_key_is_usable_as_a_file_name() {
        let repo_root = tempfile::tempdir().expect("repo tempdir");
        write_crate(&repo_root.path().join("crates/core"), true);

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(repo_root.path().join("crates/core"));

        let key = cargo_content_hash(&config);
        assert!(
            !key.contains('/') && !key.contains('\\') && !key.contains(':'),
            "a cache key must be a single path component, got: {key}",
        );
    }

    /// An external cargo root scans no reviewed tree at all, so it must not
    /// produce a commit-shaped key promising a result about that commit.
    #[test]
    fn reviewed_substrate_key_is_absent_for_an_external_cargo_root() {
        assert_eq!(
            reviewed_substrate_key_for("abc123", Path::new("/elsewhere/crate"), Path::new("/repo")),
            None,
        );
    }

    /// The reviewed branch may have moved the crate (root crate pushed into
    /// `backend/`, member renamed). The locally detected root does not exist in
    /// the snapshot then, and running cargo in a missing directory reports an
    /// execution error as the reviewed crate's verdict.
    #[tokio::test]
    async fn cargo_run_falls_back_to_the_snapshot_root_when_the_manifest_moved() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
        // Local layout: the crate lives in crates/core.
        write_crate(&repo_root.path().join("crates/core"), true);
        // Reviewed layout: the crate moved back to the repo root.
        write_crate(scan_dir.path(), true);

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(repo_root.path().join("crates/core"));
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let run = plan_cargo_run(&config).expect("plan");
        assert_eq!(
            run.cwd,
            scan_dir.path(),
            "a stale member path must not be projected into the snapshot verbatim",
        );

        // When the mapped root DOES exist in the snapshot, it still wins.
        write_crate(&scan_dir.path().join("crates/core"), true);
        let run = plan_cargo_run(&config).expect("plan");
        assert_eq!(run.cwd, scan_dir.path().join("crates/core"));
    }

    /// Defence in depth for the eligibility skip: if a cargo check is ever run
    /// directly with an external root off-HEAD, it must fail loudly instead of
    /// quietly analysing the operator's own tree.
    #[tokio::test]
    async fn cargo_run_refuses_an_external_cargo_root_off_head() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
        write_crate(outside.path(), true);

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.profile.cargo_root = Some(outside.path().to_path_buf());
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let err = match plan_cargo_run(&config) {
            Ok(run) => panic!(
                "external root off-HEAD must not resolve, got cwd {}",
                run.cwd.display()
            ),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("outside the repository"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn test_snapshot_cargo_root_maps_workspace_member() {
        let repo_root = Path::new("/repo");
        let scan_dir = Path::new("/tmp/snap");

        assert_eq!(
            snapshot_cargo_root(Path::new("/repo/crates/core"), repo_root, scan_dir),
            Some(PathBuf::from("/tmp/snap/crates/core")),
        );
        assert_eq!(
            snapshot_cargo_root(repo_root, repo_root, scan_dir),
            Some(PathBuf::from("/tmp/snap")),
        );
        // A relative cargo root is repo-root-relative by construction.
        assert_eq!(
            snapshot_cargo_root(Path::new("."), repo_root, scan_dir),
            Some(PathBuf::from("/tmp/snap")),
        );
        // Outside the repo: a snapshot of that repo can never contain it.
        assert_eq!(
            snapshot_cargo_root(Path::new("/elsewhere/crate"), repo_root, scan_dir),
            None,
        );
    }
}
