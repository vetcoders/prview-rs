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

        let output = run_command("semgrep", &args, cwd).await?;
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
                    args: &args,
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
fn build_semgrep_args<'a>(config_arg: &'a str, baseline_commit: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec![
        "scan", "--config", config_arg, "--json", "--error", "--quiet",
    ];

    if let Some(commit) = baseline_commit {
        args.push("--baseline-commit");
        args.push(commit);
    }

    args.extend([
        ".",
        "--exclude",
        "target",
        "--exclude",
        "node_modules",
        "--exclude",
        "*.min.js",
        "--exclude",
        "public_dist",
    ]);

    args
}

fn semgrep_baseline_commit(config: &Config, cwd: &Path) -> Option<String> {
    let repo = Repository::open(cwd).ok()?;
    let target = repo.resolve_target(config).ok()?;
    let head = repo.head_commit_id().ok()?;
    let target_is_checkout = head == target.commit_id;
    let dirty = worktree_has_uncommitted_changes(cwd);

    if !baseline_scan_allowed(config.security_full, dirty, target_is_checkout) {
        // `--pr` / `--remote` / fast remote-only presets analyse a *fetched*
        // ref that is not checked out. A `--baseline-commit` scan diffs the
        // working tree against the baseline, so on a clean local checkout
        // (e.g. `main`) it would diff empty and hide real findings in the
        // target. Surface the reason and fall back to a full scan.
        if !target_is_checkout && !config.security_full && !config.quiet {
            eprintln!(
                "semgrep: analysed target {} is not the checked-out commit {}; \
                 running a full scan instead of a diff-scoped baseline",
                short_oid(&target.commit_id),
                short_oid(&head),
            );
        }
        return None;
    }

    let base = repo.resolve_bases(config).ok()?.into_iter().next()?;

    if base.commit_id == target.commit_id {
        return None;
    }

    repo.merge_base(&base.commit_id, &target.commit_id).ok()
}

/// Whether semgrep may run a diff-scoped `--baseline-commit` scan.
///
/// Baseline mode diffs the *working tree* against the baseline commit, so it is
/// only sound when the analysed target is the commit currently checked out
/// (`target_is_checkout`). In remote-target modes (`--pr`, `--remote`, the fast
/// remote-only preset) the target is a fetched ref that is NOT checked out, so
/// the working tree would diff empty and mask real findings — those runs must
/// fall back to a full scan. A dirty worktree or an explicit `--security-full`
/// also forces a full scan.
fn baseline_scan_allowed(
    security_full: bool,
    worktree_dirty: bool,
    target_is_checkout: bool,
) -> bool {
    !security_full && !worktree_dirty && target_is_checkout
}

/// First 8 hex chars of a commit id for human-readable logs (oids are ASCII).
fn short_oid(id: &str) -> &str {
    &id[..id.len().min(8)]
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
        let has_exclude = |val: &str| args.windows(2).any(|w| w[0] == "--exclude" && w[1] == val);
        assert!(args.contains(&"--json"), "structured parser expects JSON");
        assert!(has_exclude("*.min.js"), "must exclude minified bundles");
        assert!(has_exclude("public_dist"), "must exclude generated site");
        assert!(has_exclude("node_modules"));
        assert!(has_exclude("target"));
        assert!(args.contains(&"auto"), "config arg threaded through");
    }

    #[test]
    fn build_semgrep_args_adds_baseline_commit_when_available() {
        let args = build_semgrep_args("auto", Some("abc123"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--baseline-commit" && w[1] == "abc123"),
            "baseline commit must be threaded to semgrep"
        );
    }

    #[test]
    fn build_semgrep_args_omits_baseline_without_merge_base() {
        let args = build_semgrep_args("auto", None);
        assert!(
            !args.contains(&"--baseline-commit"),
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

    #[test]
    fn baseline_allowed_when_target_is_checkout_and_clean() {
        // Local run whose analysed target IS the checked-out commit: diffing the
        // working tree against the baseline is sound.
        assert!(baseline_scan_allowed(false, false, true));
    }

    #[test]
    fn baseline_disallowed_when_target_not_checked_out() {
        // `--pr` / `--remote` / fast remote-only: the fetched target is not the
        // working tree, so a baseline diff would hide real findings → full scan.
        assert!(!baseline_scan_allowed(false, false, false));
    }

    #[test]
    fn baseline_disallowed_when_security_full_or_dirty() {
        // `--security-full` forces a full scan even on the checked-out target.
        assert!(!baseline_scan_allowed(true, false, true));
        // A dirty worktree cannot be trusted as a clean diff base.
        assert!(!baseline_scan_allowed(false, true, true));
    }
}
