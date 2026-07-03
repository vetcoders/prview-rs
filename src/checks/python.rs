//! Python checks (ruff, mypy, pytest)

use super::{
    Check, CheckProvenance, CheckResult, CheckStatus, TEST_TIMEOUT_SECS, find_hard_fail_signatures,
    run_command, run_command_with_timeout, tool_spawn_failure_in_output,
};
use crate::Config;
use crate::cache;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;

pub struct RuffCheck;
pub struct MypyCheck;
pub struct PytestCheck;

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
        Some(format!("ruff-{}", cache::python_hash(&config.repo_root)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command("uv", &["run", "ruff", "check", "."], &config.repo_root).await?
        } else {
            run_command("ruff", &["check", "."], &config.repo_root).await?
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
                cwd: config.repo_root.display().to_string(),
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
        Some(format!("mypy-{}", cache::python_hash(&config.repo_root)))
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command("uv", &["run", "mypy", "."], &config.repo_root).await?
        } else {
            run_command("mypy", &["."], &config.repo_root).await?
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
                cwd: config.repo_root.display().to_string(),
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

        let use_uv = which::which("uv").is_ok();
        let output = if use_uv {
            run_command_with_timeout(
                "uv",
                &["run", "pytest", "-v"],
                &config.repo_root,
                TEST_TIMEOUT_SECS,
            )
            .await?
        } else {
            run_command_with_timeout("pytest", &["-v"], &config.repo_root, TEST_TIMEOUT_SECS)
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
            provenance: Some(CheckProvenance {
                command: cmd_str.to_string(),
                tool_version: None,
                cwd: config.repo_root.display().to_string(),
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
}
