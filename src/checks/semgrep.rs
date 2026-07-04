//! Semgrep security scan check

use super::{Check, CheckEligibility, CheckResult, CheckStatus, ProvenanceBuilder, run_command};
use crate::Config;
use crate::git::Repository;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use std::path::Path;

pub struct SemgrepCheck;

#[async_trait]
impl Check for SemgrepCheck {
    fn name(&self) -> &str {
        "Semgrep scan"
    }

    fn check_eligibility(&self, _config: &Config) -> CheckEligibility {
        if which::which("semgrep").is_ok() {
            CheckEligibility::Run
        } else {
            CheckEligibility::Skip("semgrep not available".to_string())
        }
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let cwd = &config.repo_root;

        let config_path = cwd.join("semgrep.yml");
        let config_arg = if config_path.exists() {
            "semgrep.yml"
        } else {
            "auto"
        };

        let baseline_commit = semgrep_baseline_commit(config, cwd);
        let args = build_semgrep_args(config_arg, baseline_commit.as_deref());
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();

        let output = run_command("semgrep", &arg_refs, cwd).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = classify_semgrep_status(output.status.success(), &stdout, &combined);

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    cmd: "semgrep",
                    args: &arg_refs,
                    cwd,
                    output: &output,
                    combined_output: &combined,
                    started_at: &started_at,
                    finished_at: &finished_at,
                    cache_key: None,
                }
                .build(),
            ),
        })
    }
}

/// Classify a semgrep scan result.
///
/// Semgrep OSS cannot parse some modern Rust constructs (`&raw const`,
/// `unsafe extern "C"`, raw identifiers, …). When it hits one it records a
/// `PartialParsing` entry in the JSON `errors[]` array at `level: "warn"` and
/// silently skips the unparseable spans, while still exiting 0 with
/// `results: []`. The previous classifier looked for the substring "warning"
/// in the combined output, which never matched semgrep's `"level":"warn"`
/// errors, so a degraded scan was reported as a clean PASS — hiding the fact
/// that part of the tree was never analysed.
///
/// Now any non-empty `errors[]` (parse errors / partial parsing) downgrades a
/// successful scan to `Warnings`, making the degraded coverage a visible
/// review signal instead of a silent pass. A non-zero exit (real findings with
/// `--error`, or a tool crash) remains a `Failed`.
fn classify_semgrep_status(command_succeeded: bool, stdout: &str, combined: &str) -> CheckStatus {
    if !command_succeeded {
        return CheckStatus::Failed;
    }

    if semgrep_has_scan_errors(stdout) || combined.contains("warning") {
        return CheckStatus::Warnings;
    }

    CheckStatus::Passed
}

/// True when semgrep's JSON output reports any scan/parse errors (including
/// `PartialParsing`) in its `errors[]` array.
fn semgrep_has_scan_errors(stdout: &str) -> bool {
    let Some(start) = stdout.find('{') else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stdout[start..].trim()) else {
        return false;
    };
    parsed
        .get("errors")
        .and_then(|errors| errors.as_array())
        .is_some_and(|errors| !errors.is_empty())
}

/// Build the `semgrep scan` argument list. Excludes build/vendor artifacts —
/// `target`, `node_modules`, minified bundles (`*.min.js`) and the generated
/// `public_dist/` site — so the scan does not emit forever-red findings on
/// unreviewable code that no PR author can fix (vendored `dagre.min.js`
/// prototype-pollution, `public_dist` missing-integrity, …). Extracted as a
/// pure helper so the exclude set is hermetically testable without invoking the
/// semgrep binary.
fn build_semgrep_args(config_arg: &str, baseline_commit: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "scan".to_string(),
        "--config".to_string(),
        config_arg.to_string(),
        "--json".to_string(),
        "--error".to_string(),
        "--quiet".to_string(),
    ];

    if let Some(commit) = baseline_commit {
        args.push("--baseline-commit".to_string());
        args.push(commit.to_string());
    }

    args.extend(
        [
            ".",
            "--exclude",
            "target",
            "--exclude",
            "node_modules",
            "--exclude",
            "*.min.js",
            "--exclude",
            "public_dist",
        ]
        .into_iter()
        .map(String::from),
    );

    args
}

fn semgrep_baseline_commit(config: &Config, cwd: &Path) -> Option<String> {
    if config.security_full || worktree_has_uncommitted_changes(cwd) {
        return None;
    }

    let repo = Repository::open(cwd).ok()?;
    let target = repo.resolve_target(config).ok()?;
    let base = repo.resolve_bases(config).ok()?.into_iter().next()?;

    if base.commit_id == target.commit_id {
        return None;
    }

    repo.merge_base(&base.commit_id, &target.commit_id).ok()
}

fn worktree_has_uncommitted_changes(cwd: &Path) -> bool {
    let Ok(repo) = git2::Repository::discover(cwd) else {
        return true;
    };

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true);

    repo.statuses(Some(&mut opts))
        .map(|statuses| !statuses.is_empty())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;

    #[test]
    fn build_semgrep_args_excludes_vendored_and_generated_and_emits_json() {
        let args = build_semgrep_args("auto", None);
        let has_exclude = |val: &str| args.windows(2).any(|w| w == ["--exclude", val]);
        assert!(
            args.iter().any(|arg| arg == "--json"),
            "structured parser expects JSON"
        );
        assert!(has_exclude("*.min.js"), "must exclude minified bundles");
        assert!(has_exclude("public_dist"), "must exclude generated site");
        assert!(has_exclude("node_modules"));
        assert!(has_exclude("target"));
        assert!(
            args.iter().any(|arg| arg == "auto"),
            "config arg threaded through"
        );
    }

    #[test]
    fn build_semgrep_args_adds_baseline_commit_when_available() {
        let args = build_semgrep_args("auto", Some("abc123"));
        assert!(
            args.windows(2)
                .any(|window| window == ["--baseline-commit", "abc123"]),
            "baseline commit must be threaded to semgrep"
        );
    }

    #[test]
    fn build_semgrep_args_omits_baseline_without_merge_base() {
        let args = build_semgrep_args("auto", None);
        assert!(
            !args.iter().any(|arg| arg == "--baseline-commit"),
            "full-tree fallback must not pass a bogus baseline"
        );
    }

    #[test]
    fn test_semgrep_check_name() {
        let check = SemgrepCheck;
        assert_eq!(check.name(), "Semgrep scan");
    }

    #[test]
    fn partial_parsing_errors_degrade_to_warnings() {
        // Real semgrep OSS output: clean results but PartialParsing errors on
        // modern Rust constructs. Must NOT be reported as a clean PASS.
        let stdout = r#"{"version":"1.135.0","results":[],"errors":[{"code":3,"level":"warn","type":["PartialParsing",[]],"message":"Syntax error: `unsafe extern \"C\"` was unexpected"}]}"#;
        let combined = format!("{stdout}\n");
        assert_eq!(
            classify_semgrep_status(true, stdout, &combined),
            CheckStatus::Warnings
        );
    }

    #[test]
    fn clean_scan_with_no_errors_passes() {
        let stdout = r#"{"version":"1.135.0","results":[],"errors":[]}"#;
        let combined = format!("{stdout}\n");
        assert_eq!(
            classify_semgrep_status(true, stdout, &combined),
            CheckStatus::Passed
        );
    }

    #[test]
    fn non_zero_exit_is_failed() {
        let stdout = r#"{"version":"1.135.0","results":[],"errors":[]}"#;
        let combined = format!("{stdout}\n");
        assert_eq!(
            classify_semgrep_status(false, stdout, &combined),
            CheckStatus::Failed
        );
    }

    #[test]
    fn semgrep_has_scan_errors_detects_partial_parsing() {
        let with_errors = r#"{"results":[],"errors":[{"type":["PartialParsing",[]]}]}"#;
        let without_errors = r#"{"results":[],"errors":[]}"#;
        assert!(semgrep_has_scan_errors(with_errors));
        assert!(!semgrep_has_scan_errors(without_errors));
        assert!(!semgrep_has_scan_errors("not json"));
    }

    #[test]
    fn test_semgrep_check_can_run() {
        let config = test_config();
        let check = SemgrepCheck;
        let _ = check.check_eligibility(&config);
    }
}
