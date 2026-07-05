//! Semgrep security scan check

use super::{Check, CheckEligibility, CheckResult, CheckStatus, ProvenanceBuilder, run_command};
use crate::Config;
use crate::git::{Repository, ResolvedRef, git_cmd};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
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

/// An ephemeral detached `git worktree` checked out at a specific commit. Kept
/// alive for the duration of a scan; the worktree is deregistered and its files
/// removed on drop, on every path (scan success or error).
struct WorktreeSnapshot {
    repo_root: PathBuf,
    worktree_path: PathBuf,
    // Owns the enclosing temp dir; dropped after the worktree is deregistered so
    // the directory removal is the backstop for the `git worktree remove` call.
    _tmp: tempfile::TempDir,
}

impl Drop for WorktreeSnapshot {
    fn drop(&mut self) {
        // Deregister the worktree from the main repo, then prune bookkeeping.
        // `--force` is required because the checkout is detached. Errors are
        // swallowed: cleanup must be best-effort and never panic in a
        // destructor (the temp-dir removal is the backstop).
        let _ = git_cmd()
            .args(["worktree", "remove", "--force"])
            .arg(&self.worktree_path)
            .current_dir(&self.repo_root)
            .output();
        let _ = git_cmd()
            .args(["worktree", "prune"])
            .current_dir(&self.repo_root)
            .output();
    }
}

/// Create an ephemeral detached worktree of `commit` under a fresh temp dir.
fn create_worktree_snapshot(repo_root: &Path, commit: &str) -> Result<WorktreeSnapshot> {
    let tmp = tempfile::tempdir()?;
    // `git worktree add` wants a path it can create, so point it at a fresh
    // subdirectory of the temp dir rather than the (already-created) temp root.
    let worktree_path = tmp.path().join("snapshot");

    let output = git_cmd()
        .args(["worktree", "add", "--detach", "--force"])
        .arg(&worktree_path)
        .arg(commit)
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr.trim());
    }

    Ok(WorktreeSnapshot {
        repo_root: repo_root.to_path_buf(),
        worktree_path,
        _tmp: tmp,
    })
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
