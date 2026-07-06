//! Eval suite — regression testing for verdict quality.
//!
//! Tests that the PolicyEngine produces correct analysis_status and
//! merge_recommendation for representative scenarios.
//!
//! Run with: `cargo test --test eval_suite`

use prview::checks::{CheckResult, CheckStatus, SkippedCheck};
use prview::policy::engine::{AnalysisStatus, CheckEvaluation, MergeRecommendation};
use prview::policy::{PolicyConfig, PolicyMode, PolicySeverity};
use std::time::Duration;

fn make_check(name: &str, status: CheckStatus) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status,
        duration: Duration::from_secs(1),
        output: String::new(),
        cached: false,
        provenance: None,
    }
}

fn make_skipped(name: &str, reason: &str) -> SkippedCheck {
    SkippedCheck {
        id: name.to_lowercase().replace([' ', '-'], "_"),
        name: name.to_string(),
        reason: reason.to_string(),
    }
}

/// Aggregate worst status/recommendation from a list of evaluations.
fn aggregate(evals: &[CheckEvaluation]) -> (AnalysisStatus, MergeRecommendation) {
    let mut worst_confidence = AnalysisStatus::Complete;
    let mut worst_merge = MergeRecommendation::Approve;

    for eval in evals {
        if eval.confidence_impact == AnalysisStatus::Incomplete {
            worst_confidence = AnalysisStatus::Incomplete;
        } else if eval.confidence_impact == AnalysisStatus::Degraded
            && worst_confidence == AnalysisStatus::Complete
        {
            worst_confidence = AnalysisStatus::Degraded;
        }
        if eval.merge_impact == MergeRecommendation::Block {
            worst_merge = MergeRecommendation::Block;
        } else if eval.merge_impact == MergeRecommendation::ReviewRequired
            && worst_merge == MergeRecommendation::Approve
        {
            worst_merge = MergeRecommendation::ReviewRequired;
        }
    }

    (worst_confidence, worst_merge)
}

/// Evaluate a scenario through the PolicyEngine and aggregate results.
fn eval_scenario(
    policy: PolicyConfig,
    checks: &[CheckResult],
    skipped: &[SkippedCheck],
    remote_only: bool,
) -> (AnalysisStatus, MergeRecommendation) {
    // Use tempdir to construct a real Config via for_state_viewer
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_result = prview::config::Config::for_state_viewer(tmp.path());
    let mut config = match config_result {
        Ok(c) => c,
        Err(_) => {
            // fallback: init a git repo in tempdir
            git2::Repository::init(tmp.path()).expect("git init");
            prview::config::Config::for_state_viewer(tmp.path()).expect("config")
        }
    };
    config.policy = policy;
    config.remote_only = remote_only;

    let engine = prview::policy::engine::PolicyEngine::new(&config);
    let mut evals = Vec::new();

    for check in checks {
        evals.push(engine.evaluate_run(check));
    }
    for skip in skipped {
        evals.push(engine.evaluate_skip(skip));
    }

    aggregate(&evals)
}

// ── Eval cases ──────────────────────────────────────────────────────

#[test]
fn eval_clean_pr_all_pass() {
    let checks = vec![
        make_check("Cargo check", CheckStatus::Passed),
        make_check("Clippy", CheckStatus::Passed),
        make_check("Cargo test", CheckStatus::Passed),
    ];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &[], false);
    assert_eq!(status, AnalysisStatus::Complete);
    assert_eq!(rec, MergeRecommendation::Approve);
}

#[test]
fn eval_check_failure_needs_review() {
    let checks = vec![
        make_check("Cargo check", CheckStatus::Passed),
        make_check("Clippy", CheckStatus::Failed),
    ];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &[], false);
    assert_eq!(status, AnalysisStatus::Complete);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_system_error_degrades_confidence() {
    let checks = vec![
        make_check("Cargo check", CheckStatus::Error),
        make_check("Clippy", CheckStatus::Passed),
    ];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &[], false);
    assert_eq!(status, AnalysisStatus::Incomplete);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_skipped_warn_check_degrades_confidence() {
    let checks = vec![make_check("Cargo check", CheckStatus::Passed)];
    let skipped = vec![make_skipped("Clippy", "disabled for this run")];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &skipped, false);
    assert_eq!(status, AnalysisStatus::Degraded);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_skipped_block_check_is_incomplete() {
    let mut policy = PolicyConfig::default();
    policy
        .checks
        .insert("cargo_audit".to_string(), PolicySeverity::Block);

    let checks = vec![make_check("Cargo check", CheckStatus::Passed)];
    let skipped = vec![make_skipped("Cargo audit", "tool not installed")];
    let (status, rec) = eval_scenario(policy, &checks, &skipped, false);
    assert_eq!(status, AnalysisStatus::Incomplete);
    assert_eq!(rec, MergeRecommendation::Block);
}

#[test]
fn eval_required_mode_skip_is_incomplete_caveat() {
    let mut policy = PolicyConfig::default();
    policy
        .checks
        .insert("cargo_audit".to_string(), PolicySeverity::Block);

    let checks = vec![make_check("Cargo check", CheckStatus::Passed)];
    let skipped = vec![make_skipped("Cargo audit", "security disabled")];
    let (status, rec) = eval_scenario(policy, &checks, &skipped, false);
    assert_eq!(status, AnalysisStatus::Incomplete);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_required_check_skipped_at_runtime_blocks() {
    // PR #12 review #1: a check REQUIRED by policy that RAN but returned
    // Skipped (e.g. a tool that spawned then failed and was downgraded) must
    // NOT be scored as a silent pass. It routes through the same skip policy as
    // a pre-run skip, so a required+skipped runtime result blocks the gate.
    let mut policy = PolicyConfig::default();
    policy
        .checks
        .insert("cargo_audit".to_string(), PolicySeverity::Block);

    let checks = vec![
        make_check("Cargo check", CheckStatus::Passed),
        make_check("Cargo audit", CheckStatus::Skipped),
    ];
    let (status, rec) = eval_scenario(policy, &checks, &[], false);
    assert_eq!(status, AnalysisStatus::Incomplete);
    assert_eq!(rec, MergeRecommendation::Block);
}

#[test]
fn eval_warn_check_skipped_at_runtime_degrades() {
    // A non-required (default Warn) check that runs and returns Skipped is a
    // degraded signal requiring review — never a silent Approve/Complete.
    let checks = vec![
        make_check("Cargo check", CheckStatus::Passed),
        make_check("Cargo geiger", CheckStatus::Skipped),
    ];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &[], false);
    assert_eq!(status, AnalysisStatus::Degraded);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_profile_mismatch_skip_is_clean() {
    let checks = vec![make_check("Cargo check", CheckStatus::Passed)];
    let skipped = vec![make_skipped("ESLint", "profile rust")];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &skipped, false);
    assert_eq!(status, AnalysisStatus::Complete);
    assert_eq!(rec, MergeRecommendation::Approve);
}

#[test]
fn eval_fast_remote_block_check_is_degraded() {
    let mut policy = PolicyConfig::default();
    policy
        .checks
        .insert("cargo_test".to_string(), PolicySeverity::Block);

    let checks = vec![];
    let skipped = vec![make_skipped("Cargo test", "fast remote-only preset")];
    let (status, rec) = eval_scenario(policy, &checks, &skipped, true);
    assert_eq!(status, AnalysisStatus::Degraded);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_shadow_mode_never_blocks() {
    let policy = PolicyConfig {
        mode: PolicyMode::Shadow,
        ..PolicyConfig::default()
    };
    let checks = vec![
        make_check("Cargo check", CheckStatus::Failed),
        make_check("Clippy", CheckStatus::Failed),
    ];
    let (status, rec) = eval_scenario(policy, &checks, &[], false);
    assert_eq!(status, AnalysisStatus::Complete);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_block_mode_blocks_on_warn_severity() {
    let policy = PolicyConfig {
        mode: PolicyMode::Block,
        default_severity: PolicySeverity::Warn,
        ..PolicyConfig::default()
    };
    let checks = vec![make_check("Clippy", CheckStatus::Failed)];
    let (_, rec) = eval_scenario(policy, &checks, &[], false);
    assert_eq!(rec, MergeRecommendation::Block);
}

#[test]
fn eval_warnings_only_is_advisory() {
    let checks = vec![
        make_check("Clippy", CheckStatus::Warnings),
        make_check("Cargo test", CheckStatus::Passed),
    ];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &[], false);
    assert_eq!(status, AnalysisStatus::Complete);
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}

#[test]
fn eval_ignore_severity_skip_is_clean() {
    let mut policy = PolicyConfig::default();
    policy
        .checks
        .insert("cargo_geiger".to_string(), PolicySeverity::Ignore);

    let checks = vec![make_check("Cargo check", CheckStatus::Passed)];
    let skipped = vec![make_skipped("Cargo geiger", "tool not installed")];
    let (status, rec) = eval_scenario(policy, &checks, &skipped, false);
    assert_eq!(status, AnalysisStatus::Complete);
    assert_eq!(rec, MergeRecommendation::Approve);
}

#[test]
fn eval_mixed_scenario_worst_wins() {
    let checks = vec![
        make_check("Cargo check", CheckStatus::Passed),
        make_check("Clippy", CheckStatus::Warnings),
        make_check("Cargo test", CheckStatus::Error),
    ];
    let skipped = vec![make_skipped("Cargo audit", "disabled")];
    let (status, rec) = eval_scenario(PolicyConfig::default(), &checks, &skipped, false);
    // Error = Incomplete (worst confidence)
    assert_eq!(status, AnalysisStatus::Incomplete);
    // Error + Advisory = ReviewRequired (worst merge)
    assert_eq!(rec, MergeRecommendation::ReviewRequired);
}
