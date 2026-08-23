//! Semgrep security scan check

use super::{Check, CheckEligibility, CheckResult, CheckStatus, ProvenanceBuilder, run_command};
use crate::Config;
use crate::git::{Repository, ResolvedRef, WorktreeSnapshot, create_worktree_snapshot};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use serde::Deserialize;
use std::path::{Path, PathBuf};

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

        // A remote-target run (`--pr` / `--remote`) analyses a fetched commit
        // that is NOT the working tree. Scanning `config.repo_root` in place
        // would analyse the WRONG tree, so materialise the target in an
        // ephemeral detached worktree and scan that instead. `_snapshot` keeps
        // the worktree alive (and is cleaned up on drop) for the whole scan.
        let plan = match plan_semgrep_scan(config) {
            Ok(plan) => plan,
            Err(reason) => {
                // Hard blocker materialising the target: fail loud (SKIPPED with
                // a reason) instead of silently scanning the local checkout as if
                // it were the target.
                return Ok(CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Skipped,
                    duration: start.elapsed(),
                    output: reason,
                    cached: false,
                    provenance: None,
                });
            }
        };

        let cwd = plan.scan_dir.as_path();

        let config_path = cwd.join("semgrep.yml");
        let config_arg = if config_path.exists() {
            "semgrep.yml"
        } else {
            "auto"
        };

        let args = build_semgrep_args(config_arg, plan.baseline_commit.as_deref());

        let output = run_command("semgrep", &args, cwd).await?;
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = classify_semgrep_status(output.status.success(), &stdout, &combined);

        // A tool/config error (non-zero exit, no findings payload) is
        // classified Skipped above; give it a reason-carrying output instead
        // of the raw combined dump, so the gate's skip-reason plumbing
        // (policy/engine.rs reads `result.output` verbatim as the reason) shows
        // *why* it was skipped rather than a wall of stderr noise.
        let output_text = if status == CheckStatus::Skipped && !output.status.success() {
            format_tool_error_reason(output.status.code(), &stdout, &stderr)
        } else {
            combined.clone()
        };

        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: output_text,
            cached: false,
            provenance: Some(
                ProvenanceBuilder {
                    check: self.name(),
                    cmd: "semgrep",
                    args: &args,
                    cwd,
                    repo_root: &config.repo_root,
                    exit_code: output.status.code(),
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
/// Any non-empty `errors[]` (parse errors / partial parsing) downgrades a
/// successful scan to `Warnings`, making degraded coverage a visible review
/// signal instead of a silent pass.
///
/// A non-zero exit is ambiguous on its own: `--error` makes semgrep exit 1
/// when it found real results, but semgrep also exits non-zero (commonly 2)
/// on a config/tool error — an invalid ruleset, a crash — where it never
/// produced a findings payload at all. Counting the latter as a code `Failed`
/// makes a broken scanner look like a regression in the PR's own code
/// (TOOL-VS-CODE). So a non-zero exit is only `Failed` when stdout carries a
/// parsable payload with at least one actual result; otherwise it is a tool
/// error and classifies `Skipped`, mirroring the missing-tool pattern already
/// used for ruff/mypy (`checks/python.rs`).
fn classify_semgrep_status(command_succeeded: bool, stdout: &str, _combined: &str) -> CheckStatus {
    if !command_succeeded {
        return if output_has_findings_payload(stdout) {
            CheckStatus::Failed
        } else {
            CheckStatus::Skipped
        };
    }

    if output_reports_scan_errors(stdout) {
        return CheckStatus::Warnings;
    }

    CheckStatus::Passed
}

/// True when semgrep's output reports any scan/parse errors (including
/// `PartialParsing`) in its JSON `errors[]` array — i.e. part of the target
/// could not be analysed, so the reported finding set is incomplete.
///
/// Robust to trailing bytes after the JSON object: the check stores stdout and
/// stderr combined, so a stray progress/warning line may follow the JSON. A
/// streaming deserializer reads the first JSON value and ignores the rest,
/// rather than failing the whole parse the way `from_str` (which rejects
/// trailing input) would.
pub(crate) fn output_reports_scan_errors(output: &str) -> bool {
    let Some(start) = output.find('{') else {
        return false;
    };
    let mut de = serde_json::Deserializer::from_str(&output[start..]);
    let Ok(parsed) = serde_json::Value::deserialize(&mut de) else {
        return false;
    };
    parsed
        .get("errors")
        .and_then(|errors| errors.as_array())
        .is_some_and(|errors| !errors.is_empty())
}

/// True when `stdout` carries a parsable JSON payload with at least one entry
/// in `results` — i.e. semgrep actually produced findings, as opposed to a
/// non-zero exit with no results at all (config/tool error). Used to
/// distinguish a genuine `--error` exit (real findings in the code, stays
/// `Failed`) from a tool/config error exit (no payload, classifies `Skipped`
/// as a tool error rather than a code failure).
fn output_has_findings_payload(stdout: &str) -> bool {
    let Some(start) = stdout.find('{') else {
        return false;
    };
    let mut de = serde_json::Deserializer::from_str(&stdout[start..]);
    let Ok(parsed) = serde_json::Value::deserialize(&mut de) else {
        return false;
    };
    parsed
        .get("results")
        .and_then(|results| results.as_array())
        .is_some_and(|results| !results.is_empty())
}

/// First excerpt in `candidates` that carries any text.
fn first_nonempty<const N: usize>(candidates: [String; N]) -> String {
    candidates
        .into_iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or_default()
}

/// Up to five non-empty lines of raw tool output, on one line.
fn text_excerpt(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Messages from a semgrep `--json` payload's `errors[]`, if it carries any.
///
/// Field naming varies across semgrep versions, so the first of
/// `message` / `long_msg` / `short_msg` / `type` that is present wins.
fn json_errors_excerpt(stdout: &str) -> String {
    let Some(start) = stdout.find('{') else {
        return String::new();
    };
    let mut de = serde_json::Deserializer::from_str(&stdout[start..]);
    let Ok(parsed) = serde_json::Value::deserialize(&mut de) else {
        return String::new();
    };
    let Some(errors) = parsed.get("errors").and_then(|e| e.as_array()) else {
        return String::new();
    };

    errors
        .iter()
        .filter_map(|error| {
            ["message", "long_msg", "short_msg", "type"]
                .iter()
                .find_map(|field| error.get(field).and_then(|v| v.as_str()))
        })
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Human-readable skip reason for a semgrep tool/config error: the exit code
/// plus a short excerpt of whatever diagnostic the run produced — stderr when
/// there is any, otherwise the stdout payload's `errors[]`, otherwise raw
/// stdout — so a reviewer (and the policy engine, which reads
/// `CheckResult.output` verbatim as the skip reason) sees why the check did not
/// run rather than a raw stdout/stderr dump or a generic sentence.
fn format_tool_error_reason(exit_code: Option<i32>, stdout: &str, stderr: &str) -> String {
    // Under `--json` semgrep reports config/rule failures in the stdout payload's
    // `errors[]` and can leave stderr completely empty, so reading stderr alone
    // discarded the only diagnostic there was and the policy engine received the
    // generic "no findings payload" sentence as its skip reason.
    let excerpt = first_nonempty([
        text_excerpt(stderr),
        json_errors_excerpt(stdout),
        text_excerpt(stdout),
    ]);

    let exit_label = exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if excerpt.is_empty() {
        format!(
            "semgrep exited {exit_label} with no findings payload (tool/config error, not a code failure)"
        )
    } else {
        format!(
            "semgrep exited {exit_label} with no findings payload (tool/config error, not a code failure): {excerpt}"
        )
    }
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

/// A resolved plan for where a semgrep scan runs and how it is baselined.
struct SemgrepScanPlan {
    /// Directory to run semgrep in — the working tree in place, or an ephemeral
    /// worktree snapshot of a remote target.
    scan_dir: PathBuf,
    /// Merge-base for a diff-scoped `--baseline-commit` scan, or `None` for a
    /// full scan.
    baseline_commit: Option<String>,
    /// Kept alive so the ephemeral worktree is not cleaned up before the scan
    /// finishes; `None` for an in-place scan.
    _snapshot: Option<WorktreeSnapshot>,
}

/// Decide where semgrep should scan.
///
/// When the analysed target is the checked-out commit, scan the working tree in
/// place. When it is a fetched remote target (`--pr` / `--remote`) that is NOT
/// checked out, materialise it in an ephemeral detached worktree and scan that —
/// otherwise the scan analyses the local checkout instead of the target. In the
/// snapshot HEAD == target and the tree is clean, so a diff-scoped baseline
/// against the merge-base is sound again.
///
/// Returns `Err(reason)` when a remote target cannot be materialised, so the
/// caller can fail loud (SKIPPED) rather than scan the wrong tree.
fn plan_semgrep_scan(config: &Config) -> std::result::Result<SemgrepScanPlan, String> {
    let repo_root = config.repo_root.clone();

    let Ok(repo) = Repository::open(&repo_root) else {
        // Not a git repository (or unreadable) — scan in place with no baseline.
        return Ok(SemgrepScanPlan {
            scan_dir: repo_root,
            baseline_commit: None,
            _snapshot: None,
        });
    };

    let (Ok(target), Ok(head)) = (repo.resolve_target(config), repo.head_commit_id()) else {
        // Refs did not resolve — fall back to an in-place scan; the in-place
        // baseline helper degrades to a full scan on the same failure.
        return Ok(SemgrepScanPlan {
            baseline_commit: semgrep_baseline_commit(config, &repo_root),
            scan_dir: repo_root,
            _snapshot: None,
        });
    };

    if head == target.commit_id {
        // Working tree IS the target: in-place scan with the existing baseline.
        return Ok(SemgrepScanPlan {
            baseline_commit: semgrep_baseline_commit(config, &repo_root),
            scan_dir: repo_root,
            _snapshot: None,
        });
    }

    // Remote target: materialise it in an ephemeral detached worktree.
    let snapshot = create_worktree_snapshot(&repo_root, &target.commit_id).map_err(|e| {
        format!(
            "semgrep: could not create an ephemeral worktree for target {} ({e}); \
             skipping instead of scanning the local checkout",
            short_oid(&target.commit_id),
        )
    })?;

    let baseline = snapshot_baseline_commit(&repo, config, &target);

    Ok(SemgrepScanPlan {
        scan_dir: snapshot.worktree_path.clone(),
        baseline_commit: baseline,
        _snapshot: Some(snapshot),
    })
}

/// Baseline commit for a scan whose working tree IS the target (in place). The
/// merge-base enables a diff-scoped `--baseline-commit` scan; `None` forces a
/// full scan (dirty worktree, `--security-full`, or no distinct base).
fn semgrep_baseline_commit(config: &Config, cwd: &Path) -> Option<String> {
    let repo = Repository::open(cwd).ok()?;
    let target = repo.resolve_target(config).ok()?;
    let head = repo.head_commit_id().ok()?;
    let target_is_checkout = head == target.commit_id;
    let dirty = worktree_has_uncommitted_changes(cwd);

    if !baseline_scan_allowed(
        config.security_full,
        dirty,
        target_is_checkout,
        config.current_only,
    ) {
        return None;
    }

    merge_base_for_baseline(&repo, config, &target)
}

/// Baseline commit for an ephemeral worktree snapshot of a remote target. The
/// snapshot has HEAD == target and a clean tree, so a diff-scoped baseline is
/// sound unless the run opts out (`--security-full`). A `None` result runs a
/// full scan of the target's state.
fn snapshot_baseline_commit(
    repo: &Repository,
    config: &Config,
    target: &ResolvedRef,
) -> Option<String> {
    // In the snapshot the target IS the checkout and the tree is clean.
    if !baseline_scan_allowed(config.security_full, false, true, config.current_only) {
        return None;
    }
    merge_base_for_baseline(repo, config, target)
}

/// Shared merge-base resolution: the merge-base of the single resolved base and
/// the target, or `None` when a diff-scoped scan would be unsound.
///
/// `semgrep --baseline-commit` diffs against exactly ONE commit. With more than
/// one resolved base (the default probe resolves develop/main/master, and
/// `generate_diffs` builds a diff for each) baselining only the first base would
/// silently suppress a finding that is pre-existing versus that base but NEW
/// versus another — even though the artifact pack contains the other base's diff
/// (R3-15). Rather than baseline the wrong single base, fall back to a full scan
/// whenever the run resolved anything other than exactly one base. Reconciling a
/// true multi-baseline scan is deliberately out of scope here.
fn merge_base_for_baseline(
    repo: &Repository,
    config: &Config,
    target: &ResolvedRef,
) -> Option<String> {
    let bases = repo.resolve_bases(config).ok()?;
    // Exactly one resolved base is the only sound shape for a single
    // `--baseline-commit`; 0 or 2+ fall back to a full scan.
    let [base] = bases.as_slice() else {
        return None;
    };
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
/// also forces a full scan. `--current-only` deliberately drops the bases to
/// scan the whole current state, so it must never be diff-scoped against a
/// resolved default base.
fn baseline_scan_allowed(
    security_full: bool,
    worktree_dirty: bool,
    target_is_checkout: bool,
    current_only: bool,
) -> bool {
    !security_full && !worktree_dirty && target_is_checkout && !current_only
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
    use crate::git::git_cmd;

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
    fn clean_scan_with_warning_word_in_stderr_passes() {
        let stdout = r#"{"version":"1.135.0","results":[],"errors":[]}"#;
        let combined = format!("{stdout}\nscanned /tmp/warning-fixture/src/lib.rs\n");
        assert_eq!(
            classify_semgrep_status(true, stdout, &combined),
            CheckStatus::Passed
        );
    }

    #[test]
    fn non_zero_exit_with_findings_payload_is_failed() {
        // `--error` makes semgrep exit 1 when it found real results: that is a
        // genuine code failure, not a tool error.
        let stdout = r#"{"version":"1.135.0","results":[{"check_id":"rust.lang.security.blah","path":"src/x.rs","start":{"line":1}}],"errors":[]}"#;
        let combined = format!("{stdout}\n");
        assert_eq!(
            classify_semgrep_status(false, stdout, &combined),
            CheckStatus::Failed
        );
    }

    #[test]
    fn non_zero_exit_with_no_payload_is_skipped_as_tool_error() {
        // exit 2 with no results at all (invalid ruleset / crash): a tool or
        // config error, not a code regression — must not be Failed
        // (TOOL-VS-CODE, verify-ledger claim #8).
        let stdout = "";
        let combined = "Invalid configuration file\nsemgrep: error while validating rules\n";
        assert_eq!(
            classify_semgrep_status(false, stdout, combined),
            CheckStatus::Skipped
        );
    }

    #[test]
    fn non_zero_exit_with_empty_results_array_is_skipped_as_tool_error() {
        // A JSON payload that parses but carries zero results is still "no
        // findings" — a non-zero exit alongside it is a tool error, not a
        // code failure hiding behind an empty result set.
        let stdout = r#"{"version":"1.135.0","results":[],"errors":[]}"#;
        let combined = format!("{stdout}\n");
        assert_eq!(
            classify_semgrep_status(false, stdout, &combined),
            CheckStatus::Skipped
        );
    }

    #[test]
    fn output_reports_scan_errors_detects_partial_parsing() {
        let with_errors = r#"{"results":[],"errors":[{"type":["PartialParsing",[]]}]}"#;
        let without_errors = r#"{"results":[],"errors":[]}"#;
        assert!(output_reports_scan_errors(with_errors));
        assert!(!output_reports_scan_errors(without_errors));
        assert!(!output_reports_scan_errors("not json"));
    }

    #[test]
    fn output_reports_scan_errors_tolerates_trailing_stderr() {
        // The check stores stdout+stderr combined, so a stderr progress line may
        // follow the JSON object. A strict `from_str` would reject the trailing
        // bytes and miss the errors; the streaming reader must still see them.
        let combined = "{\"results\":[],\"errors\":[{\"type\":[\"PartialParsing\",[]]}]}\nsome semgrep stderr noise\n";
        assert!(output_reports_scan_errors(combined));
    }

    #[test]
    fn output_has_findings_payload_detects_nonempty_results() {
        let with_results = r#"{"results":[{"check_id":"x"}],"errors":[]}"#;
        let empty_results = r#"{"results":[],"errors":[]}"#;
        assert!(output_has_findings_payload(with_results));
        assert!(!output_has_findings_payload(empty_results));
        assert!(!output_has_findings_payload("not json"));
        assert!(!output_has_findings_payload(""));
    }

    #[test]
    fn format_tool_error_reason_includes_exit_code_and_stderr_excerpt() {
        let reason = format_tool_error_reason(
            Some(2),
            "",
            "Invalid configuration file\nsemgrep: error while validating rules\n",
        );
        assert!(reason.contains('2'), "reason must surface the exit code");
        assert!(
            reason.contains("Invalid configuration file"),
            "reason must surface a stderr excerpt"
        );
        assert!(
            reason.contains("tool/config error"),
            "reason must name it as a tool error, not a code failure"
        );
    }

    #[test]
    fn format_tool_error_reason_handles_missing_exit_code_and_empty_stderr() {
        let reason = format_tool_error_reason(None, "", "");
        assert!(reason.contains("unknown"));
        assert!(reason.contains("tool/config error"));
    }

    #[test]
    fn format_tool_error_reason_reads_json_errors_from_stdout() {
        // With `--json`, semgrep puts its diagnostics in the stdout payload's
        // `errors[]` and can leave stderr empty. Reading stderr alone threw the
        // only explanation away and handed the policy engine the generic "no
        // findings payload" line as the skip reason.
        let stdout = r#"{"results":[],"errors":[
            {"type":"InvalidRuleSchemaError","message":"invalid rule: missing key `pattern`"},
            {"type":"SemgrepError","long_msg":"config auto is unreachable"}
        ]}"#;
        let reason = format_tool_error_reason(Some(2), stdout, "");

        assert!(
            reason.contains("invalid rule: missing key `pattern`"),
            "the JSON error must reach the skip reason: {reason}"
        );
        assert!(
            reason.contains("config auto is unreachable"),
            "a second error must not be dropped: {reason}"
        );
        assert!(reason.contains("tool/config error"), "{reason}");
    }

    #[test]
    fn format_tool_error_reason_falls_back_to_non_json_stdout() {
        // A crashing semgrep can print a traceback on stdout with nothing on
        // stderr; that text is still the only diagnostic there is.
        let reason = format_tool_error_reason(Some(2), "Traceback (most recent call last)\n", "");
        assert!(
            reason.contains("Traceback"),
            "non-JSON stdout is still a diagnostic: {reason}"
        );
    }

    #[test]
    fn format_tool_error_reason_prefers_stderr_when_both_carry_text() {
        let reason = format_tool_error_reason(Some(2), "{\"results\":[],\"errors\":[]}", "boom\n");
        assert!(reason.contains("boom"), "{reason}");
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
        assert!(baseline_scan_allowed(false, false, true, false));
    }

    #[test]
    fn baseline_disallowed_when_target_not_checked_out() {
        // `--pr` / `--remote` / fast remote-only: the fetched target is not the
        // working tree, so a baseline diff would hide real findings → full scan.
        assert!(!baseline_scan_allowed(false, false, false, false));
    }

    #[test]
    fn baseline_disallowed_when_security_full_or_dirty() {
        // `--security-full` forces a full scan even on the checked-out target.
        assert!(!baseline_scan_allowed(true, false, true, false));
        // A dirty worktree cannot be trusted as a clean diff base.
        assert!(!baseline_scan_allowed(false, true, true, false));
    }

    #[test]
    fn baseline_disallowed_when_current_only() {
        // `--current-only` drops the bases to scan the whole current state, so
        // semgrep must never diff-scope against a resolved default base — even on
        // a clean, checked-out target.
        assert!(!baseline_scan_allowed(false, false, true, true));
    }

    // ── R2-10: ephemeral worktree snapshot for remote targets ──────────

    fn run_git(repo: &Path, args: &[&str]) {
        let status = git_cmd()
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
        let output = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn worktree_count(repo: &Path) -> usize {
        let output = git_cmd()
            .args(["worktree", "list"])
            .current_dir(repo)
            .output()
            .expect("worktree list");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    }

    #[test]
    fn worktree_snapshot_materialises_target_and_cleans_up_on_drop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        let earlier = write_commit(tmp.path(), "a.txt", "one\n");
        let _head = write_commit(tmp.path(), "b.txt", "two\n");

        let worktree_path;
        {
            let snapshot =
                create_worktree_snapshot(tmp.path(), &earlier).expect("snapshot creation");
            worktree_path = snapshot.worktree_path.clone();

            // The snapshot is checked out at the earlier commit: a.txt present,
            // b.txt (added later) absent.
            assert!(snapshot.worktree_path.join("a.txt").exists());
            assert!(!snapshot.worktree_path.join("b.txt").exists());
            // The main repo now has a second, registered worktree.
            assert_eq!(worktree_count(tmp.path()), 2);
        }

        // Dropped: the worktree directory is removed and deregistered.
        assert!(
            !worktree_path.exists(),
            "worktree dir must be removed on drop"
        );
        assert_eq!(
            worktree_count(tmp.path()),
            1,
            "worktree must be deregistered on drop"
        );
    }

    #[test]
    fn worktree_snapshot_errors_on_unknown_commit_without_leaking() {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        let _initial = write_commit(tmp.path(), "a.txt", "one\n");
        let before = worktree_count(tmp.path());

        let result =
            create_worktree_snapshot(tmp.path(), "0000000000000000000000000000000000000000");
        assert!(result.is_err(), "a bogus commit must fail to materialise");
        assert_eq!(
            worktree_count(tmp.path()),
            before,
            "a failed worktree add must not leave a registered worktree"
        );
    }

    // ── R3-15: diff-scoped baseline only with exactly one resolved base ──

    #[test]
    fn merge_base_is_diff_scoped_with_a_single_base() {
        use crate::config::{test_config_builder, test_generic_profile};

        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        let base_commit = write_commit(tmp.path(), "a.txt", "one\n");
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        let _target = write_commit(tmp.path(), "b.txt", "two\n");

        let config = test_config_builder()
            .repo_root(tmp.path())
            .target(Some("feature"))
            .bases(&["main"])
            .profile(test_generic_profile())
            .build();

        let repo = Repository::open(tmp.path()).expect("open repo");
        let resolved_target = repo.resolve_target(&config).expect("resolve target");

        let baseline = merge_base_for_baseline(&repo, &config, &resolved_target);
        assert_eq!(
            baseline.as_deref(),
            Some(base_commit.as_str()),
            "a single resolved base must diff-scope against its merge-base"
        );
    }

    #[test]
    fn merge_base_baseline_matches_artifact_diff_base_after_base_advances() {
        use crate::config::{test_config_builder, test_generic_profile};

        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        let merge_base = write_commit(tmp.path(), "own.rs", "pub fn own() -> u8 { 1 }\n");
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        let _target = write_commit(tmp.path(), "own.rs", "pub fn own() -> u8 { 2 }\n");
        run_git(tmp.path(), &["checkout", "-q", "main"]);
        let _advance_one = write_commit(
            tmp.path(),
            "unrelated.rs",
            "pub fn unrelated_one() -> u8 { 1 }\n",
        );
        let _advance_two = write_commit(
            tmp.path(),
            "unrelated.rs",
            "pub fn unrelated_one() -> u8 { 1 }\npub fn unrelated_two() -> u8 { 2 }\n",
        );
        run_git(tmp.path(), &["checkout", "-q", "feature"]);

        let config = test_config_builder()
            .repo_root(tmp.path())
            .target(Some("feature"))
            .bases(&["main"])
            .profile(test_generic_profile())
            .build();

        let repo = Repository::open(tmp.path()).expect("open repo");
        let resolved_target = repo.resolve_target(&config).expect("resolve target");
        let resolved_bases = repo.resolve_bases(&config).expect("resolve bases");
        let diff_bases = repo.resolve_diff_bases(&resolved_target, &resolved_bases, true);

        assert_eq!(
            merge_base_for_baseline(&repo, &config, &resolved_target).as_deref(),
            diff_bases.first().map(|base| base.commit_id.as_str())
        );
        assert_eq!(
            diff_bases.first().map(|base| base.commit_id.as_str()),
            Some(merge_base.as_str())
        );
    }

    #[test]
    fn merge_base_falls_back_to_full_scan_with_multiple_bases() {
        use crate::config::{test_config_builder, test_generic_profile};

        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        let _base_commit = write_commit(tmp.path(), "a.txt", "one\n");
        // A second base ref pointing at the same commit as `main`; both resolve.
        run_git(tmp.path(), &["branch", "develop", "main"]);
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        let _target = write_commit(tmp.path(), "b.txt", "two\n");

        let config = test_config_builder()
            .repo_root(tmp.path())
            .target(Some("feature"))
            .bases(&["main", "develop"])
            .profile(test_generic_profile())
            .build();

        let repo = Repository::open(tmp.path()).expect("open repo");
        let resolved_target = repo.resolve_target(&config).expect("resolve target");
        // Sanity: both bases really do resolve, so this is a genuine multi-base run.
        assert_eq!(
            repo.resolve_bases(&config).expect("resolve bases").len(),
            2,
            "fixture must resolve two bases"
        );

        assert_eq!(
            merge_base_for_baseline(&repo, &config, &resolved_target),
            None,
            "more than one resolved base must fall back to a full scan (R3-15)"
        );
    }

    #[test]
    fn plan_scans_snapshot_when_target_is_not_checked_out() {
        use crate::config::{test_config_builder, test_generic_profile};

        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        let earlier = write_commit(tmp.path(), "a.txt", "one\n");
        let target = write_commit(tmp.path(), "b.txt", "two\n");
        // Move the working tree back so HEAD != target (mirrors a remote target
        // that is fetched but not checked out).
        run_git(tmp.path(), &["checkout", "-q", &earlier]);

        let config = test_config_builder()
            .repo_root(tmp.path())
            .target(Some(target.as_str()))
            .profile(test_generic_profile())
            .build();

        let plan = plan_semgrep_scan(&config).expect("plan");
        assert_ne!(
            plan.scan_dir,
            tmp.path(),
            "a non-checked-out target must scan the snapshot, not the local checkout"
        );
        assert!(
            plan._snapshot.is_some(),
            "the scan dir must be backed by an ephemeral snapshot"
        );
        // The snapshot is checked out at the target commit: b.txt is present.
        assert!(plan.scan_dir.join("b.txt").exists());
    }
}
