//! Python checks (ruff, mypy, pytest)

use super::{
    Check, CheckProvenance, CheckResult, CheckStatus, TEST_TIMEOUT_SECS, find_hard_fail_signatures,
    plan_check_run, run_command, run_command_with_timeout, tool_spawn_failure_in_output,
};
use crate::Config;
use crate::cache;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;

pub struct RuffCheck;
pub struct MypyCheck;
pub struct PytestCheck;

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

        let plan = plan_check_run(config)?;
        let run_dir = &plan.scan_dir;

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command("uv", &["run", "ruff", "check", "."], run_dir).await?
        } else {
            run_command("ruff", &["check", "."], run_dir).await?
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
            provenance: Some(CheckProvenance {
                command: cmd_str.to_string(),
                tool_version: None,
                cwd: run_dir.display().to_string(),
                exit_code: output.status.code(),
                started_at,
                finished_at,
                hard_fail_signatures: find_hard_fail_signatures(&combined),
                cache_key: self.cache_key(config),
            }),
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

        let plan = plan_check_run(config)?;
        let run_dir = &plan.scan_dir;

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command("uv", &["run", "mypy", "."], run_dir).await?
        } else {
            run_command("mypy", &["."], run_dir).await?
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
            provenance: Some(CheckProvenance {
                command: cmd_str.to_string(),
                tool_version: None,
                cwd: run_dir.display().to_string(),
                exit_code: output.status.code(),
                started_at,
                finished_at,
                hard_fail_signatures: find_hard_fail_signatures(&combined),
                cache_key: self.cache_key(config),
            }),
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
        let plan = plan_check_run(config)?;
        let run_dir = &plan.scan_dir;

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command_with_timeout("uv", &["run", "pytest", "-v"], run_dir, TEST_TIMEOUT_SECS)
                .await?
        } else {
            run_command_with_timeout("pytest", &["-v"], run_dir, TEST_TIMEOUT_SECS).await?
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
            provenance: Some(CheckProvenance {
                command: cmd_str.to_string(),
                tool_version: None,
                cwd: run_dir.display().to_string(),
                exit_code: output.status.code(),
                started_at,
                finished_at,
                hard_fail_signatures: find_hard_fail_signatures(&combined),
                cache_key: self.cache_key(config),
            }),
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

    #[test]
    fn test_ruff_check_name() {
        let check = RuffCheck;
        assert_eq!(check.name(), "Ruff");
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
