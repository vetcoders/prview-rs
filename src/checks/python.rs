//! Python checks (ruff, mypy, pytest)

use super::{
    Check, CheckProvenance, CheckResult, CheckStatus, TEST_TIMEOUT_SECS, find_hard_fail_signatures,
    off_head_target_commit, plan_check_run, run_command_with_env, run_command_with_timeout_and_env,
    tool_spawn_failure_in_output,
};
use crate::Config;
use crate::cache;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct RuffCheck;
pub struct MypyCheck;
pub struct PytestCheck;

/// Skip reason when the REVIEWED commit is not a Python project.
///
/// `config.profile` describes the local checkout. When a target removes the last
/// Python project and source files, the checkout still says "Python" and the
/// checks were still scheduled — into a snapshot that has no Python in it.
/// Pytest is where that hurts: it exits 5 for "no tests collected", a blocking
/// failure attributed to a target the check no longer applies to. Ruff and Mypy
/// pass vacuously, which is a green signal for something never examined; both
/// are answers about a question that should not have been asked.
///
/// The same shape as `missing_reviewed_cargo_manifest`, and answered from git —
/// the snapshot carries exactly this tree, so no worktree is materialised to ask.
///
/// Fail open at every step: a question git cannot answer must not become a skip,
/// so an unreadable repo or a failed walk leaves the check running.
fn missing_reviewed_python_project(config: &Config) -> Option<String> {
    let commit = off_head_target_commit(config)?;
    let repo = crate::git::Repository::open(&config.repo_root).ok()?;

    // A pyproject.toml is an explicit project declaration and settles it alone,
    // exactly as `runs_python_checks` treats it locally.
    if repo
        .regular_file_at_commit(&commit, "pyproject.toml")
        .unwrap_or(true)
    {
        return None;
    }
    if repo
        .any_file_at_commit(&commit, crate::config::is_runtime_python_path)
        .unwrap_or(true)
    {
        return None;
    }

    let short = &commit[..commit.len().min(8)];
    Some(format!(
        "commit {short} has no pyproject.toml and no Python source — not a Python project",
    ))
}

/// Where a python check must execute, plus the environment it needs there.
struct PythonRun {
    /// Directory to run the tool in — the reviewed snapshot in `--pr`/`--remote`
    /// mode, the local checkout otherwise.
    cwd: PathBuf,
    /// Extra child environment (`UV_PROJECT_ENVIRONMENT`), empty for a local run.
    env: Vec<(String, String)>,
    /// Ephemeral snapshot, kept alive until the check finishes.
    _snapshot: Option<crate::git::WorktreeSnapshot>,
}

/// Resolve where a python check runs, and isolate uv from the operator's
/// environment when that place is a target snapshot.
///
/// `create_worktree_snapshot` symlinks the checkout's `.venv` into the snapshot
/// so a review does not reinstall every dependency. `uv run` synchronises the
/// project environment before executing, so a reviewed commit that adds, drops
/// or pins dependencies differently would mutate the developer's ACTIVE
/// environment through that symlink — a review is a read of someone's branch,
/// never a write to their machine. `UV_PROJECT_ENVIRONMENT` moves the sync into
/// a prview-owned directory ([`Config::uv_env_dir_for`]), so the reviewed
/// dependency set is still installed and judged, just not on top of the
/// operator's.
///
/// That environment is per REVIEWED COMMIT, not per repository. `uv run` syncs
/// before executing and releases the environment lock while the child command
/// runs, so two prview processes reviewing different commits of one repo would
/// take turns installing incompatible dependency sets into the same directory —
/// each one resynchronising (and removing packages) under the other's running
/// pytest. A commit-scoped path makes those two reviews independent while
/// keeping the environment warm across runs of the SAME commit, which is the
/// case that pays for itself (re-review, `--watch`).
///
/// A local review (target == `HEAD`) sets no environment override and behaves
/// exactly as before.
fn plan_python_run(config: &Config) -> Result<PythonRun> {
    let plan = plan_check_run(config)?;
    let env = if plan.scan_dir == config.repo_root {
        Vec::new()
    } else {
        let env_dir = config.uv_env_dir_for(&reviewed_env_token(config, &plan.scan_dir));
        mark_and_prune_uv_envs(&config.uv_env_root(), &env_dir);
        vec![(
            "UV_PROJECT_ENVIRONMENT".to_string(),
            env_dir.display().to_string(),
        )]
    };
    Ok(PythonRun {
        cwd: plan.scan_dir,
        env,
        _snapshot: plan._snapshot,
    })
}

/// Name of the environment for the substrate this run analyses.
///
/// The reviewed commit IS the dependency set, so it names the environment. When
/// no off-`HEAD` commit resolves while the scan still happens elsewhere (an
/// injected scan dir), the snapshot path stands in: unknown provenance must not
/// collapse two different substrates onto one environment.
fn reviewed_env_token(config: &Config, scan_dir: &Path) -> String {
    off_head_target_commit(config)
        .unwrap_or_else(|| format!("snapshot-{}", cache::key_token(&scan_dir.to_string_lossy())))
}

/// Environments kept regardless of age — the working set of a repo under review.
const UV_ENVS_KEPT: usize = 3;

/// How long an environment is untouchable after its last use. A review does not
/// run for a day, so anything older cannot belong to a live run.
const UV_ENV_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Marker refreshed on every use, so reuse (which only writes deep inside the
/// environment) still counts as recent activity.
const UV_ENV_USED_MARKER: &str = ".prview-used";

/// Serialises marking and pruning across processes. A plain file, so
/// [`prune_uv_envs`]'s directory filter passes over it.
const UV_PRUNE_LOCK: &str = ".prview-prune.lock";

/// Record this environment as used and drop the ones that are neither recent nor
/// part of the working set.
///
/// Per-commit isolation trades one directory per repository for one per reviewed
/// commit, so without a bound a busy repository would leave a virtualenv behind
/// for every commit ever reviewed — hundreds of megabytes each. The bound is
/// deliberately timid: the newest [`UV_ENVS_KEPT`] survive whatever their age,
/// and nothing used within [`UV_ENV_MIN_AGE`] is touched, so a concurrent (or
/// merely slow) review cannot have its environment deleted underneath it.
///
/// Age alone does not make the bound safe, because two reviews run
/// concurrently: one process can read an environment's timestamp just before
/// another refreshes it, and then delete the directory once that other process
/// has already started `uv run`. Marking and pruning are therefore one critical
/// section, serialised across processes by [`UV_PRUNE_LOCK`] — no other prview
/// can observe this root between our mark and our sweep.
///
/// The lock is opportunistic. Pruning is housekeeping, so a root already locked
/// by a live review is simply left to that review: we still record OUR use,
/// which is what protects this environment from the next sweep, and skip the
/// sweep itself. (`prune_uv_envs` re-reads each candidate immediately before
/// removing it, so a mark that lands outside the lock still wins.)
///
/// Nothing is created here: an absent root means no environment exists yet, and
/// pre-creating the directory would leave uv an empty non-environment to reject.
fn mark_and_prune_uv_envs(root: &Path, env_dir: &Path) {
    if !root.is_dir() {
        return;
    }
    let lock = crate::storage::acquire_lock_at(&root.join(UV_PRUNE_LOCK)).ok();
    if env_dir.is_dir() {
        let _ = std::fs::write(env_dir.join(UV_ENV_USED_MARKER), b"");
    }
    if lock.is_some() {
        prune_uv_envs(root, UV_ENVS_KEPT, UV_ENV_MIN_AGE);
    }
    drop(lock);
}

/// Pure half of [`mark_and_prune_uv_envs`]: remove environments beyond the
/// `keep` most recently used that have also been idle for at least `min_age`.
fn prune_uv_envs(root: &Path, keep: usize, min_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut envs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| (last_used(&p), p))
        .collect();

    // Newest first, so the tail is what the working set does not cover.
    envs.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
    let now = std::time::SystemTime::now();
    let idle_for = |used| now.duration_since(used).unwrap_or_default();
    for (used, path) in envs.into_iter().skip(keep) {
        if idle_for(used) < min_age {
            continue;
        }
        // Re-read immediately before deleting. The listing above is a snapshot,
        // and a review that could not take the prune lock still marks the
        // environment it is about to use; that mark must beat a verdict formed
        // from a stat taken before it landed.
        if idle_for(last_used(&path)) < min_age {
            continue;
        }
        let _ = std::fs::remove_dir_all(path);
    }
}

/// When an environment was last used: the marker if this prview wrote one, the
/// directory's own timestamp otherwise (an environment from an older prview, or
/// one created but never reused).
fn last_used(env_dir: &Path) -> std::time::SystemTime {
    let marker = env_dir.join(UV_ENV_USED_MARKER);
    std::fs::metadata(&marker)
        .or_else(|_| std::fs::metadata(env_dir))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Classify a ruff run from its exit status and combined output.
///
/// A missing tool is a setup gap, not a lint failure. When uv wraps a ruff that
/// is not installed it emits "error: Failed to spawn: `ruff`" with a non-zero
/// exit; that must classify as Skipped (mirroring [`mypy_status`], PR #1
/// b1697d4) rather than a lint Failed that would falsely dent the gate in every
/// Python repo without ruff. A genuine non-zero exit with lint findings stays
/// Failed.
fn ruff_status(success: bool, combined: &str) -> CheckStatus {
    if success {
        CheckStatus::Passed
    } else if tool_spawn_failure_in_output(combined) {
        CheckStatus::Skipped
    } else {
        CheckStatus::Failed
    }
}

#[async_trait]
impl Check for RuffCheck {
    fn name(&self) -> &str {
        "Ruff"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.runs_python_checks() {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if let Some(reason) = missing_reviewed_python_project(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        let repo = crate::git::Repository::open(&config.repo_root).ok()?;
        let target = repo.resolve_target(config).ok()?;
        let head = repo.head_commit_id().ok()?;
        if head == target.commit_id {
            Some(format!("ruff-{}", cache::python_hash(&config.repo_root)))
        } else {
            Some(format!("ruff-{}", target.commit_id))
        }
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let plan = plan_python_run(config)?;
        let run_dir = &plan.cwd;

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command_with_env("uv", &["run", "ruff", "check", "."], run_dir, &plan.env).await?
        } else {
            run_command_with_env("ruff", &["check", "."], run_dir, &plan.env).await?
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = ruff_status(output.status.success(), &combined);

        let cmd_str = if use_uv {
            "uv run ruff check ."
        } else {
            "ruff check ."
        };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: cmd_str.to_string(),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

/// Classify a mypy run from its exit status and combined output.
///
/// A missing tool is a setup gap, not a type error: uv emits
/// "error: Failed to spawn: `mypy` / No such file or directory" when mypy is
/// not installed, which would otherwise be misread as a type error -> Skipped.
fn mypy_status(success: bool, combined: &str) -> CheckStatus {
    if success {
        CheckStatus::Passed
    } else if tool_spawn_failure_in_output(combined) {
        // uv emits "error: Failed to spawn: `mypy`" when mypy is not installed.
        // Match only that unambiguous launcher marker — never a bare "no such
        // file or directory", which mypy itself prints in real diagnostics
        // (matching it would turn a genuine failure into an invisible pass).
        CheckStatus::Skipped
    } else if combined.contains("error:") {
        CheckStatus::Failed
    } else {
        CheckStatus::Warnings
    }
}

#[async_trait]
impl Check for MypyCheck {
    fn name(&self) -> &str {
        "Mypy"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.runs_python_checks() {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if let Some(reason) = missing_reviewed_python_project(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, config: &Config) -> Option<String> {
        let repo = crate::git::Repository::open(&config.repo_root).ok()?;
        let target = repo.resolve_target(config).ok()?;
        let head = repo.head_commit_id().ok()?;
        if head == target.commit_id {
            Some(format!("mypy-{}", cache::python_hash(&config.repo_root)))
        } else {
            Some(format!("mypy-{}", target.commit_id))
        }
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let plan = plan_python_run(config)?;
        let run_dir = &plan.cwd;

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command_with_env("uv", &["run", "mypy", "."], run_dir, &plan.env).await?
        } else {
            run_command_with_env("mypy", &["."], run_dir, &plan.env).await?
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = mypy_status(output.status.success(), &combined);

        let cmd_str = if use_uv { "uv run mypy ." } else { "mypy ." };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: cmd_str.to_string(),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

#[async_trait]
impl Check for PytestCheck {
    fn name(&self) -> &str {
        "Pytest"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.runs_python_checks() {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if let Some(reason) = missing_reviewed_python_project(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if config.is_fast_remote_only_standard() && !config.run_tests {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_tests {
            return super::CheckEligibility::Skip("tests disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    // Tests are not cached - they should always run fresh
    fn cache_key(&self, _config: &Config) -> Option<String> {
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        // Run from the reviewed substrate, not the local checkout: with a PR or
        // remote target, `config.repo_root` still holds whatever branch happens
        // to be checked out locally, so pytest would report a foreign branch's
        // failures against the PR (PRV-PYTEST-HEAD). Ruff, Mypy and the sibling
        // test runner Vitest all resolve their cwd through `plan_check_run`;
        // Pytest was the sole outlier. For a local review the plan resolves back
        // to `repo_root`, so that path is unchanged.
        let plan = plan_python_run(config)?;
        let run_dir = &plan.cwd;

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command_with_timeout_and_env(
                "uv",
                &["run", "pytest", "-v"],
                run_dir,
                TEST_TIMEOUT_SECS,
                &plan.env,
            )
            .await?
        } else {
            run_command_with_timeout_and_env(
                "pytest",
                &["-v"],
                run_dir,
                TEST_TIMEOUT_SECS,
                &plan.env,
            )
            .await?
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        let cmd_str = if use_uv {
            "uv run pytest -v"
        } else {
            "pytest -v"
        };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: cmd_str.to_string(),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{test_config_builder, test_python_profile};

    fn create_test_config(has_pyproject: bool, run_lint: bool, run_tests: bool) -> Config {
        test_config_builder()
            .profile(test_python_profile(has_pyproject))
            .run_lint(run_lint)
            .run_tests(run_tests)
            .do_fetch(false)
            .use_cache(false)
            .create_zip(false)
            .build()
    }

    /// Two commits: the reviewed one carries no Python at all, the checked-out
    /// one does. Returns (repo, reviewed commit).
    fn repo_whose_target_dropped_python() -> (tempfile::TempDir, String) {
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

        // Reviewed commit: a pure Rust tree, no Python whatsoever.
        std::fs::write(root.join("README.md"), "rust only\n").expect("write");
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "no python here"]);
        let target = String::from_utf8(
            git_cmd()
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_string();

        // Checked-out commit: the Python project the local profile detects.
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"x\"\n").expect("write");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("src/app.py"), "def main():\n    pass\n").expect("write");
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "add python"]);

        (tmp, target)
    }

    /// A target that is not a Python project must not be judged by Python
    /// checks. Pytest is the sharp edge: it exits 5 for "no tests collected",
    /// and that blocking failure was attributed to a target the check does not
    /// apply to at all.
    #[test]
    fn python_checks_do_not_run_against_a_target_without_python() {
        let (repo, target) = repo_whose_target_dropped_python();
        let mut config = create_test_config(true, true, true);
        config.repo_root = repo.path().to_path_buf();
        config.target = Some(target);

        let reason = missing_reviewed_python_project(&config)
            .expect("a target with no Python is not a Python project");
        assert!(
            reason.contains("not a Python project"),
            "the skip must say why: {reason}",
        );

        for eligibility in [
            PytestCheck.check_eligibility(&config),
            RuffCheck.check_eligibility(&config),
            MypyCheck.check_eligibility(&config),
        ] {
            assert_eq!(
                eligibility,
                super::super::CheckEligibility::Skip(reason.clone()),
                "every Python check must skip with the reviewed-tree reason",
            );
        }
    }

    /// The guard must not manufacture skips: a target that still carries Python
    /// keeps running, and so does a local review, where the checkout IS the
    /// target and git is never consulted.
    #[test]
    fn a_target_that_still_has_python_keeps_running() {
        let (repo, _dropped) = repo_whose_target_dropped_python();
        let mut config = create_test_config(true, true, true);
        config.repo_root = repo.path().to_path_buf();

        // HEAD carries the Python project, so a review of it is not off-HEAD at
        // all and the guard stays out of the way.
        assert_eq!(missing_reviewed_python_project(&config), None);
        assert_eq!(
            PytestCheck.check_eligibility(&config),
            super::super::CheckEligibility::Run,
        );

        config.target = Some("main".to_string());
        assert_eq!(missing_reviewed_python_project(&config), None);
    }

    #[test]
    fn test_ruff_check_name() {
        let check = RuffCheck;
        assert_eq!(check.name(), "Ruff");
    }

    /// A review must never write into the operator's environment. The snapshot
    /// symlinks their `.venv`, and `uv run` syncs the project environment before
    /// executing — so an off-HEAD python check has to be pointed at a
    /// prview-owned environment instead of the symlinked one.
    #[test]
    fn python_run_off_head_isolates_uv_from_the_operator_environment() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let run = plan_python_run(&config).expect("plan");

        assert_eq!(run.cwd, scan_dir.path());
        assert_eq!(
            run.env,
            vec![(
                "UV_PROJECT_ENVIRONMENT".to_string(),
                config
                    .uv_env_dir_for(&reviewed_env_token(&config, scan_dir.path()))
                    .display()
                    .to_string()
            )],
            "an off-HEAD python check must sync into a prview-owned environment",
        );
        let env_dir = PathBuf::from(&run.env[0].1);
        assert!(
            !env_dir.starts_with(repo_root.path()),
            "the reviewed sync must not reach the operator's checkout (its .venv is symlinked \
             into the snapshot)",
        );
        assert!(
            !env_dir.starts_with(scan_dir.path()),
            "an environment inside the throwaway snapshot is reinstalled on every run",
        );
        assert!(
            env_dir.starts_with(config.uv_env_root()),
            "the environment stays inside the repo's prview-owned root",
        );
    }

    /// One environment per repository was still shared state: two prview
    /// processes reviewing different commits synced incompatible dependency sets
    /// into the same directory, each resynchronising under the other's running
    /// checks. Different substrates must get different environments.
    #[test]
    fn uv_environments_are_separated_per_reviewed_substrate() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let first_snapshot = tempfile::tempdir().expect("first snapshot");
        let second_snapshot = tempfile::tempdir().expect("second snapshot");

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();

        config.scan_dir_override = Some(first_snapshot.path().to_path_buf());
        let first = plan_python_run(&config).expect("plan");
        config.scan_dir_override = Some(second_snapshot.path().to_path_buf());
        let second = plan_python_run(&config).expect("plan");

        assert_ne!(
            first.env, second.env,
            "two reviews of different substrates must not share one uv environment",
        );
        // Same substrate, same environment: reuse is what keeps this affordable.
        config.scan_dir_override = Some(first_snapshot.path().to_path_buf());
        assert_eq!(plan_python_run(&config).expect("plan").env, first.env);
    }

    /// Per-commit isolation trades one directory per repo for one per reviewed
    /// commit, so the working set has to be bounded — a virtualenv per commit
    /// ever reviewed is hundreds of megabytes each.
    #[test]
    fn stale_uv_environments_are_pruned_outside_the_working_set() {
        let root = tempfile::tempdir().expect("uv-env root");
        let mut envs = Vec::new();
        for name in ["one", "two", "three", "four"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            // Distinct marker mtimes, newest last.
            std::fs::write(dir.join(UV_ENV_USED_MARKER), b"").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            envs.push(dir);
        }

        // Nothing recent is ever removed, however many there are.
        prune_uv_envs(root.path(), 1, Duration::from_secs(3600));
        for dir in &envs {
            assert!(dir.is_dir(), "a live environment must survive: {dir:?}");
        }

        // Past the age floor, only the working set stays.
        prune_uv_envs(root.path(), 2, Duration::ZERO);
        assert!(!envs[0].is_dir() && !envs[1].is_dir(), "stale envs stay");
        assert!(
            envs[2].is_dir() && envs[3].is_dir(),
            "the newest environments are the working set",
        );
    }

    /// The age floor alone does not make pruning safe: two reviews run at once,
    /// and one can read an environment's timestamp just before the other
    /// refreshes it, then delete the directory after that other review's
    /// `uv run` has begun. While another live process holds the root, this one
    /// records its own use and touches nothing else.
    #[test]
    fn a_review_holding_the_prune_lock_keeps_the_sweep_to_itself() {
        let root = tempfile::tempdir().expect("uv-env root");
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        let mut envs = Vec::new();
        for name in ["one", "two", "three", "four", "five"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            // Idle well past the age floor and beyond the working set, so an
            // unguarded sweep would delete them.
            let marker = std::fs::File::create(dir.join(UV_ENV_USED_MARKER)).unwrap();
            marker.set_modified(long_ago).unwrap();
            envs.push(dir);
        }

        // Another live prview is mid-sweep: our own pid is unquestionably alive,
        // so the lock cannot be mistaken for an abandoned one.
        std::fs::write(
            root.path().join(UV_PRUNE_LOCK),
            format!(
                "{}:{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ),
        )
        .unwrap();

        mark_and_prune_uv_envs(root.path(), &envs[0]);

        for dir in &envs {
            assert!(
                dir.is_dir(),
                "a locked root belongs to the review holding it: {dir:?}",
            );
        }
        assert!(
            envs[0].join(UV_ENV_USED_MARKER).exists(),
            "our own use must still be recorded — that is what protects it next sweep",
        );
        assert!(
            root.path().join(UV_PRUNE_LOCK).is_file(),
            "a lock we never acquired must not be cleared on the way out",
        );
    }

    /// Pruning must never be what creates the directory tree: uv rejects an
    /// existing directory that is not a valid environment.
    #[test]
    fn marking_creates_nothing_when_no_environment_exists_yet() {
        let home = tempfile::tempdir().expect("home");
        let root = home.path().join("uv-env/repo");
        let env_dir = root.join("commit");

        mark_and_prune_uv_envs(&root, &env_dir);

        assert!(!root.exists(), "an absent root must stay absent");
        assert!(!env_dir.exists(), "uv creates the environment, not prview");
    }

    /// A local review is unchanged: the operator's own environment, no override.
    #[test]
    fn python_run_local_target_is_unchanged() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();

        let run = plan_python_run(&config).expect("plan");

        assert_eq!(run.cwd, repo_root.path());
        assert!(
            run.env.is_empty(),
            "a local review must keep using the checkout's own environment",
        );
    }

    #[test]
    fn test_mypy_check_name() {
        let check = MypyCheck;
        assert_eq!(check.name(), "Mypy");
    }

    #[test]
    fn test_pytest_check_name() {
        let check = PytestCheck;
        assert_eq!(check.name(), "Pytest");
    }

    #[test]
    fn test_ruff_check_can_run_with_pyproject_and_lint() {
        let config = create_test_config(true, true, false);
        let check = RuffCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_ruff_check_cannot_run_without_pyproject() {
        let config = create_test_config(false, true, false);
        let check = RuffCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_ruff_check_cannot_run_without_lint() {
        let config = create_test_config(true, false, false);
        let check = RuffCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_mypy_check_can_run_with_pyproject_and_lint() {
        let config = create_test_config(true, true, false);
        let check = MypyCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_mypy_check_cannot_run_without_pyproject() {
        let config = create_test_config(false, true, false);
        let check = MypyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_mypy_check_cannot_run_without_lint() {
        let config = create_test_config(true, false, false);
        let check = MypyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_pytest_check_can_run_with_pyproject_and_tests() {
        let config = create_test_config(true, false, true);
        let check = PytestCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_pytest_check_cannot_run_without_pyproject() {
        let config = create_test_config(false, false, true);
        let check = PytestCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_pytest_check_cannot_run_without_tests_flag() {
        let config = create_test_config(true, false, false);
        let check = PytestCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_ruff_check_cache_key() {
        let config = create_test_config(true, true, false);
        let check = RuffCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
        assert!(key.unwrap().starts_with("ruff-"));
    }

    #[test]
    fn test_mypy_check_cache_key() {
        let config = create_test_config(true, true, false);
        let check = MypyCheck;
        let key = check.cache_key(&config);
        assert!(key.is_some());
        assert!(key.unwrap().starts_with("mypy-"));
    }

    #[test]
    fn test_pytest_check_no_cache_key() {
        let config = create_test_config(true, false, true);
        let check = PytestCheck;
        let key = check.cache_key(&config);
        assert!(key.is_none());
    }

    // ── ruff missing-tool => Skipped, real lint failure => Failed ──

    #[test]
    fn test_ruff_status_spawn_fail_is_skipped() {
        // uv wrapping a missing ruff emits this; it must be Skipped (parity
        // with mypy), never a lint Failed that dents the gate in every Python
        // repo without ruff.
        let combined =
            "\nerror: Failed to spawn: `ruff`\n  Caused by: No such file or directory (os error 2)";
        assert_eq!(
            ruff_status(false, combined),
            CheckStatus::Skipped,
            "missing ruff must classify as Skipped, not Failed"
        );
    }

    #[test]
    fn test_ruff_status_command_not_found_is_skipped() {
        assert_eq!(
            ruff_status(false, "ruff: command not found"),
            CheckStatus::Skipped,
            "a bare 'command not found' missing ruff must be Skipped"
        );
    }

    #[test]
    fn test_ruff_status_real_lint_failure_is_failed() {
        let combined = "src/x.py:1:1: F401 [*] `os` imported but unused\nFound 1 error.\n";
        assert_eq!(
            ruff_status(false, combined),
            CheckStatus::Failed,
            "genuine lint findings must classify as Failed"
        );
    }

    #[test]
    fn test_ruff_status_success_is_passed() {
        assert_eq!(ruff_status(true, "All checks passed!"), CheckStatus::Passed);
    }

    // ── PV-01: mypy missing-tool => Skipped, real type error => Failed ──

    #[test]
    fn test_mypy_status_spawn_fail_is_skipped() {
        let combined =
            "\nerror: Failed to spawn: `mypy`\n  Caused by: No such file or directory (os error 2)";
        assert_eq!(
            mypy_status(false, combined),
            CheckStatus::Skipped,
            "uv spawn-fail must classify as Skipped, not Failed"
        );
    }

    #[test]
    fn test_mypy_status_real_type_error_is_failed() {
        let combined = "src/x.py:3: error: Incompatible return value type\nFound 1 error in 1 file";
        assert_eq!(
            mypy_status(false, combined),
            CheckStatus::Failed,
            "a real ': error:' line must classify as Failed"
        );
    }

    #[test]
    fn test_mypy_status_real_error_with_enoent_text_is_failed() {
        // P1 regression: a genuine mypy failure whose text contains "no such
        // file or directory" must stay Failed, not be misread as a missing tool.
        let combined = "src/a.py:10: error: Cannot find module: No such file or directory\nFound 1 error in 1 file";
        assert_eq!(
            mypy_status(false, combined),
            CheckStatus::Failed,
            "a real failure containing 'no such file or directory' must stay Failed"
        );
    }

    #[test]
    fn test_mypy_status_success_is_passed() {
        assert_eq!(
            mypy_status(true, "Success: no issues found"),
            CheckStatus::Passed
        );
    }

    use std::path::Path;

    fn run_git(repo: &Path, args: &[&str]) {
        let status = crate::git::git_cmd()
            .args(args)
            .current_dir(repo)
            .status()
            .expect("git command");
        assert!(status.success(), "git {args:?} failed with {status}");
    }

    fn write_commit(repo: &Path, name: &str, body: &str) -> String {
        std::fs::write(repo.join(name), body).expect("write fixture");
        run_git(repo, &["add", name]);
        run_git(
            repo,
            &[
                "-c",
                "user.name=prview test",
                "-c",
                "user.email=prview@example.test",
                "commit",
                "-m",
                name,
            ],
        );
        let output = crate::git::git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Cache keys must follow the SUBSTRATE, not the local working tree.
    ///
    /// The cached-result lookup runs before the shared target snapshot exists,
    /// so a key derived from the local tree would let a `--pr` run hit the entry
    /// a previous local run stored — serving the local checkout's verdict as the
    /// reviewed commit's. The snapshot-backed language checks key on the target
    /// commit id whenever it differs from `HEAD`; this locks that in.
    #[test]
    fn language_cache_keys_do_not_share_entries_across_substrates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_path = tmp.path();
        run_git(repo_path, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo_path.join("pyproject.toml"),
            "[project]\nname = \"test\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        run_git(repo_path, &["add", "pyproject.toml"]);
        let first = write_commit(repo_path, "main.py", "def hello():\n    pass\n");
        let second = write_commit(
            repo_path,
            "main.py",
            "import os\n\ndef hello():\n    pass\n",
        );

        let config_for = |target: Option<&str>| {
            let mut builder = test_config_builder()
                .profile(test_python_profile(true))
                .run_lint(true)
                .run_tests(true)
                .do_fetch(false)
                .repo_root(repo_path.to_path_buf());
            if let Some(target) = target {
                builder = builder.target(Some(target));
            }
            builder.build()
        };

        // HEAD sits on `second`; a local review keys on the local tree hash.
        let local_key = RuffCheck
            .cache_key(&config_for(None))
            .expect("local cache key");

        // Same checkout, but the reviewed target is an older commit: the check
        // will scan a snapshot of `first`, so its key must name `first`.
        let off_head_key = RuffCheck
            .cache_key(&config_for(Some(first.as_str())))
            .expect("off-HEAD cache key");

        assert!(
            off_head_key.contains(&first),
            "an off-HEAD key must name the analysed commit, got {off_head_key}"
        );
        assert_ne!(
            local_key, off_head_key,
            "a local run and a run on a fetched target must never share a cache entry"
        );
        assert!(
            !local_key.contains(&second),
            "a local review keys on the working tree, which HEAD alone does not describe"
        );

        // Two different targets must not collide with each other either.
        let other_target_key = RuffCheck
            .cache_key(&config_for(Some(second.as_str())))
            .expect("second-target cache key");
        assert_ne!(off_head_key, other_target_key);

        // Mypy shares the shape; guard it too so the pair cannot drift apart.
        assert_ne!(
            MypyCheck.cache_key(&config_for(None)),
            MypyCheck.cache_key(&config_for(Some(first.as_str()))),
        );
    }

    #[tokio::test]
    async fn test_ruff_runs_on_fetched_target_in_remote_mode() {
        if which::which("ruff").is_err() && which::which("uv").is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_path = tmp.path();
        run_git(repo_path, &["init", "-q", "-b", "main"]);

        // Write pyproject.toml so Ruff eligibility passes
        std::fs::write(
            repo_path.join("pyproject.toml"),
            "[project]\nname = \"test\"\nversion = \"0.1.0\"\n\n[tool.ruff]",
        )
        .unwrap();
        run_git(repo_path, &["add", "pyproject.toml"]);

        // 1. Commit clean state
        let clean_content = "def hello():\n    print('hello')\n";
        let clean_commit = write_commit(repo_path, "main.py", clean_content);

        // 2. Commit dirty state with unused import
        let dirty_content = "import os\n\ndef hello():\n    print('hello')\n";
        let dirty_commit = write_commit(repo_path, "main.py", dirty_content);

        // Scenario A: HEAD is checked out at clean_commit (working tree clean),
        // but target is dirty_commit. Ruff must analyze dirty_commit and report failure.
        run_git(repo_path, &["checkout", "-q", "-f", &clean_commit]);

        let config_a = test_config_builder()
            .profile(test_python_profile(true))
            .run_lint(true)
            .target(Some(dirty_commit.as_str()))
            .repo_root(repo_path.to_path_buf())
            .build();

        let check = RuffCheck;
        let result_a = check.run(&config_a).await.expect("ruff run scenario A");
        assert_eq!(
            result_a.status,
            CheckStatus::Failed,
            "Ruff must fail because fetched target commit has an unused import. Output: {}",
            result_a.output
        );

        // Scenario B: HEAD is checked out at dirty_commit (working tree dirty),
        // but target is clean_commit. Ruff must analyze clean_commit and pass.
        run_git(repo_path, &["checkout", "-q", "-f", &dirty_commit]);

        let config_b = test_config_builder()
            .profile(test_python_profile(true))
            .run_lint(true)
            .target(Some(clean_commit.as_str()))
            .repo_root(repo_path.to_path_buf())
            .build();

        let result_b = check.run(&config_b).await.expect("ruff run scenario B");
        assert_eq!(
            result_b.status,
            CheckStatus::Passed,
            "Ruff must pass because fetched target commit is clean. Output: {}",
            result_b.output
        );
    }

    /// PRV-PYTEST-HEAD regression: Pytest must run in the reviewed substrate
    /// (`plan.scan_dir`), never in `config.repo_root`.
    ///
    /// With a PR/remote target, `repo_root` still holds whatever branch happens
    /// to be checked out locally, so the pre-fix code reported a foreign
    /// branch's test failures against the PR. The fixture makes the two
    /// directories disagree on purpose: `repo_root` holds a FAILING test and the
    /// scan dir a PASSING one, so running in the wrong place is not merely
    /// observable — it flips the verdict.
    #[tokio::test]
    async fn test_pytest_runs_in_scan_dir_not_repo_root() {
        if which::which("pytest").is_err() && which::which("uv").is_err() {
            return;
        }

        // repo_root == the stale local checkout: its test FAILS.
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        std::fs::write(
            repo_root.path().join("test_stale_local.py"),
            "def test_from_repo_root():\n    assert False, 'pytest ran in repo_root'\n",
        )
        .unwrap();

        // scan_dir == the reviewed target snapshot: its test PASSES.
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
        std::fs::write(
            scan_dir.path().join("test_reviewed_head.py"),
            "def test_from_scan_dir():\n    assert True\n",
        )
        .unwrap();

        let mut config = test_config_builder()
            .profile(test_python_profile(true))
            .run_tests(true)
            .repo_root(repo_root.path().to_path_buf())
            .do_fetch(false)
            .use_cache(false)
            .build();
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let result = PytestCheck.run(&config).await.expect("pytest run");

        assert_eq!(
            result.status,
            CheckStatus::Passed,
            "Pytest must run the reviewed snapshot's passing test, not repo_root's \
             failing one. Output: {}",
            result.output
        );
        assert!(
            !result.output.contains("pytest ran in repo_root"),
            "Pytest executed in repo_root instead of the reviewed scan dir. Output: {}",
            result.output
        );

        // Provenance must not claim a cwd the run never used.
        let cwd = result.provenance.expect("provenance").cwd;
        assert_eq!(
            std::fs::canonicalize(&cwd).unwrap(),
            std::fs::canonicalize(scan_dir.path()).unwrap(),
            "provenance cwd must report the reviewed scan dir"
        );
    }
}
