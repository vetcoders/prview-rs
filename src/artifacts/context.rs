//! DashboardContext and its sibling view types, plus build_dashboard_context().

use super::*;

/// Delta compared to a previous run (read from `latest/report.json`).
pub(crate) struct PreviousRunDelta {
    pub checks_passed_before: usize,
    pub checks_failed_before: usize,
    pub findings_before: usize,
    pub breaking_before: usize,
    pub quality_pass_before: bool,
}

/// A single historical run extracted from a previous report.json.
pub(crate) struct HistoricalRun {
    pub timestamp: String,
    pub checks_passed: usize,
    pub checks_failed: usize,
    pub checks_warned: usize,
    pub quality_pass: bool,
    pub findings_count: usize,
}

/// PRV-204: Flaky score for a single check across historical runs.
///
/// A check is "flaky" if its status flips between pass/warn/fail across runs.
/// Flaky score = transitions / (runs - 1), as a fraction 0.0..1.0.
pub(crate) struct FlakyCheckScore {
    pub check_id: String,
    pub check_name: String,
    /// 0.0 = perfectly stable, 1.0 = flips every run
    pub flaky_score: f64,
    pub total_runs: usize,
    /// Number of status transitions across runs
    pub transitions: usize,
    /// Last N statuses for sparkline (newest first), e.g. ["PASS","FAIL","PASS"]
    pub last_statuses: Vec<String>,
    /// "low" (2-3 runs), "medium" (4-7 runs), "high" (8+ runs)
    pub confidence: &'static str,
}

/// PRV-205: Diff-aware lint metrics for a single lint check.
///
/// Issues are classified as "new" (in changed files) vs "legacy" (pre-existing)
/// based on cross-referencing lint output file paths with the diff file list.
pub(crate) struct LintMetrics {
    pub check_name: String,
    pub new_issues: usize,
    pub legacy_issues: usize,
    pub total_issues: usize,
    pub changed_files_with_issues: Vec<String>,
}

/// All extra data the dashboard needs beyond Config/Diff/CheckResult/Heuristics.
pub(crate) struct DashboardContext {
    pub verdict: &'static str,
    pub analysis_status: crate::policy::engine::AnalysisStatus,
    pub merge_recommendation: crate::policy::engine::MergeRecommendation,
    pub allow_merge: bool,
    pub quality_pass: bool,
    pub policy_allow_merge: bool,
    pub recommended_merge: bool,
    pub review_caveats: Vec<String>,
    pub quality_failures: Vec<String>,
    pub introduced_quality_failures: Vec<String>,
    pub preexisting_quality_failures: Vec<String>,
    pub mixed_quality_failures: Vec<String>,
    pub unclassified_quality_failures: Vec<String>,
    pub quality_failure_details: Vec<QualityFailureDetail>,
    pub policy_mode: &'static str,
    pub blocking_issues: Vec<String>,
    pub check_gates: Vec<CheckGateEntry>,
    pub breaking: Vec<BreakingFinding>,
    pub coverage: CoverageDelta,
    pub findings: Vec<DashboardFinding>,
    pub per_file_diff_files: Vec<String>,
    pub skipped_checks: Vec<crate::checks::SkippedCheck>,
    pub previous_run: Option<PreviousRunDelta>,
    pub run_history: Vec<HistoricalRun>,
    pub flaky_scores: Vec<FlakyCheckScore>,
    pub lint_metrics: Vec<LintMetrics>,
    pub ownership_map: Vec<(String, String)>,
    pub risk_scores: Vec<signal::FileRiskScore>,
    pub i18n_delta: Option<signal::I18nDelta>,
}

pub(crate) fn build_dashboard_context(input: DashboardContextInput<'_>) -> DashboardContext {
    let DashboardContextInput {
        config,
        checks,
        heuristics,
        inline,
        breaking,
        coverage,
        diff_dir,
        skipped_checks,
        out_dir,
        diffs,
        ownership_map,
    } = input;
    use crate::policy::engine::{AnalysisStatus, MergeRecommendation, PolicyEngine};

    let engine = PolicyEngine::new(config);
    let policy_summary = engine.evaluate_all(checks, &skipped_checks);

    // THREAD 5: derive the SAME pre-existing-aware effective outcome the merge
    // gate uses, from the SAME shared function, so the dashboard verdict can
    // never contradict MERGE_GATE.json. The classification of which failures are
    // purely pre-existing must be computed before the effective outcome, so it
    // is hoisted here (also reused for quality_pass below).
    let quality_failures = build_quality_failure_summary(checks, &inline.dashboard_findings);
    let preexisting_quality_failure_names: std::collections::BTreeSet<&str> = quality_failures
        .preexisting_quality_failures
        .iter()
        .map(String::as_str)
        .collect();
    let outcome = compute_effective_policy_outcome(
        &policy_summary.evaluations,
        &preexisting_quality_failure_names,
    );

    let mut check_gates = Vec::new();
    let mut blocking_issues = outcome.blocking_issues.clone();
    let mut review_caveats = outcome.advisory_caveats.clone();
    let mut worst_confidence = outcome.worst_confidence;
    let mut worst_merge = outcome.worst_merge;

    for (eval, effective_eval) in policy_summary
        .evaluations
        .iter()
        .zip(&outcome.effective_evals)
    {
        check_gates.push(CheckGateEntry {
            name: eval.name.clone(),
            id: eval.check_id.clone(),
            blocking: matches!(effective_eval.merge_impact, MergeRecommendation::Block),
            class: gate_class_to_str(eval.gate_class),
            severity: policy_severity_to_str(eval.severity),
        });
    }

    // Heuristics gate — only add if not already present via synthetic check in all_checks
    if !checks
        .iter()
        .any(|c| check_id_from_name(&c.name) == "heuristics_loctree")
    {
        let severity = config.policy.severity_for("heuristics_loctree");
        let (status_class, dead, cycles) = if let Some(h) = heuristics {
            let dead = h.summary.dead_exports;
            let cycles = h.summary.circular_imports;
            if h.summary.total_files == 0 {
                (GateClass::Skip, dead, cycles)
            } else {
                let class = if dead > 0 || cycles > 0 {
                    GateClass::Info
                } else {
                    GateClass::Pass
                };
                (class, dead, cycles)
            }
        } else {
            (GateClass::Skip, 0, 0)
        };
        let blocking = config.policy.is_blocking(severity, status_class);
        if blocking {
            blocking_issues.push(format!(
                "Loctree heuristics (dead_exports={}, cycles={})",
                dead, cycles
            ));
        }
        let name = if let Some(h) = heuristics {
            format!(
                "Loctree heuristics (dead={}, cycles={})",
                h.summary.dead_exports, h.summary.circular_imports
            )
        } else {
            "Loctree heuristics (skipped)".to_string()
        };
        check_gates.push(CheckGateEntry {
            name,
            id: "heuristics_loctree".to_string(),
            blocking,
            class: gate_class_to_str(status_class),
            severity: policy_severity_to_str(severity),
        });
    }

    // Inline findings blocking — THREAD 7: gate on introduced/unclassified
    // findings, not the raw error count, so the dashboard agrees with the merge
    // gate and a pre-existing-only scan does not block.
    let inline_severity = config.policy.severity_for("inline_findings");
    let inline_class = effective_inline_gate_class(inline);
    let inline_blocking = config.policy.is_blocking(inline_severity, inline_class);
    if inline_blocking {
        blocking_issues.push(format!("INLINE_FINDINGS ({})", inline.status));
    }

    let policy_allow_merge = blocking_issues.is_empty();

    // quality_pass: only new/indeterminate failures count — pre-existing ones
    // are not introduced by this diff and should not block the gate.
    // (`quality_failures` is computed once, above, and shared with the effective
    // outcome.)
    let quality_pass = !quality_failures.has_new_failures();

    if !quality_pass && worst_merge == MergeRecommendation::Approve {
        worst_merge = MergeRecommendation::ReviewRequired;
    }
    if !quality_pass && worst_confidence == AnalysisStatus::Complete {
        worst_confidence = AnalysisStatus::Degraded;
    }

    review_caveats.extend(build_review_caveats(
        &breaking,
        &coverage,
        inline.findings_count,
    ));
    review_caveats.extend(rust_quality_review_caveats(config, checks));
    review_caveats.extend(cargo_audit_review_caveats(checks));
    review_caveats.extend(skipped_requested_security_review_caveats(
        config,
        checks,
        &skipped_checks,
    ));
    // Surface pre-existing failures as review caveats (informational, non-blocking)
    if !quality_failures.preexisting_quality_failures.is_empty() {
        let names = quality_failures.preexisting_quality_failures.join(", ");
        review_caveats.push(format!(
            "Pre-existing quality failures (not from this diff): {}",
            names
        ));
    }

    let findings = inline.dashboard_findings.clone();
    // Discover per-file diff files
    let per_file_dir = diff_dir.join("per-file-diffs");
    let per_file_diff_files = if per_file_dir.exists() {
        fs::read_dir(&per_file_dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "patch"))
                    .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // PRV-104: Load previous run delta from `latest/report.json` (defensive)
    let previous_run = load_previous_run_delta(out_dir);

    // PRV-201: Load historical run data for trends (defensive, up to 20 runs)
    let mut run_history = load_run_history(out_dir, 20);

    // A4 fix: Prepend synthetic current run (report.json doesn't exist yet)
    {
        let current_ts = out_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let current_run = HistoricalRun {
            timestamp: current_ts.clone(),
            checks_passed: checks
                .iter()
                .filter(|c| c.status == crate::checks::CheckStatus::Passed)
                .count(),
            checks_failed: checks
                .iter()
                .filter(|c| {
                    matches!(
                        c.status,
                        crate::checks::CheckStatus::Failed | crate::checks::CheckStatus::Error
                    )
                })
                .count(),
            checks_warned: checks
                .iter()
                .filter(|c| c.status == crate::checks::CheckStatus::Warnings)
                .count(),
            quality_pass,
            findings_count: findings.len(),
        };
        run_history.retain(|r| r.timestamp != current_ts);
        run_history.insert(0, current_run);
    }

    // PRV-204: Compute flaky scores from historical per-check data
    let flaky_scores = compute_flaky_scores(out_dir, 20);

    // PRV-205: Compute diff-aware lint metrics
    let lint_metrics = compute_lint_metrics(checks, diffs);

    // B4: Compute file risk scores
    let risk_scores = signal::compute_file_risk_scores_with_root(
        diffs,
        &coverage,
        &breaking,
        Some(&config.repo_root),
    );
    let risk_heatmap = signal::compute_risk_heatmap(diffs, &risk_scores);
    if risk_heatmap.risk_level == "high" && !risk_heatmap.zones.is_empty() {
        let top_zones = risk_heatmap
            .zones
            .iter()
            .take(3)
            .map(|zone| {
                format!(
                    "{} ({} files, churn {})",
                    zone.name, zone.files_touched, zone.total_churn
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        review_caveats.push(format!("High-risk PR surface: {top_zones}"));
        if worst_merge == MergeRecommendation::Approve {
            worst_merge = MergeRecommendation::ReviewRequired;
        }
        if worst_confidence == AnalysisStatus::Complete {
            worst_confidence = AnalysisStatus::Degraded;
        }
    }

    let semantic_findings = signal::detect_orphaned_resource_delete(diffs);
    if !semantic_findings.is_empty() {
        review_caveats.push(format!(
            "{} semantic finding{} require manual review",
            semantic_findings.len(),
            if semantic_findings.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
        if worst_merge == MergeRecommendation::Approve {
            worst_merge = MergeRecommendation::ReviewRequired;
        }
    }

    // B2: Compute i18n parity delta
    let i18n_delta = signal::compute_i18n_delta(diffs, &config.repo_root);
    // Derive the scalar decision fields from the FINAL axes (after every
    // review/risk bump above) through the single coherent source, so
    // `allow_merge` can never contradict the verdict (PV-03).
    let decision = derive_decision(worst_confidence, worst_merge, quality_pass);
    let recommended_merge = decision.recommended_merge;

    DashboardContext {
        verdict: decision.verdict,
        analysis_status: worst_confidence,
        merge_recommendation: worst_merge,
        allow_merge: decision.allow_merge,
        quality_pass,
        policy_allow_merge,
        recommended_merge,
        review_caveats,
        quality_failures: quality_failures.quality_failures,
        introduced_quality_failures: quality_failures.introduced_quality_failures,
        preexisting_quality_failures: quality_failures.preexisting_quality_failures,
        mixed_quality_failures: quality_failures.mixed_quality_failures,
        unclassified_quality_failures: quality_failures.unclassified_quality_failures,
        quality_failure_details: quality_failures.details,
        policy_mode: config.policy.mode_str(),
        blocking_issues,
        check_gates,
        breaking,
        coverage,
        findings,
        per_file_diff_files,
        skipped_checks,
        previous_run,
        run_history,
        flaky_scores,
        lint_metrics,
        ownership_map,
        risk_scores,
        i18n_delta,
    }
}
