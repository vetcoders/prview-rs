//! MERGE_GATE.md generation via the policy engine.

use super::*;

pub(super) fn generate_merge_gate(input: MergeGateInput<'_>) -> Result<()> {
    use crate::policy::engine::{AnalysisStatus, MergeRecommendation, PolicyEngine};
    use serde_json::json;
    use std::collections::BTreeSet;
    let MergeGateInput {
        dir,
        config,
        checks,
        heuristics,
        inline,
        breaking,
        coverage,
        diffs,
        skipped_checks,
        resolved_target,
        resolved_bases,
        clean_comparison,
    } = input;

    let engine = PolicyEngine::new(config);
    let policy_summary = engine.evaluate_all(checks, skipped_checks);
    let quality_failures =
        build_quality_failure_summary(checks, &inline.dashboard_findings, &clean_comparison);
    let preexisting_quality_failure_names = quality_failures
        .preexisting_quality_failures
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    // Shared effective evaluation: the pre-existing downgrade plus axis/issue
    // computation lives in ONE place so the dashboard context derives the exact
    // same verdict from the exact same result (THREAD 5 — verdict parity).
    let outcome = compute_effective_policy_outcome(
        &policy_summary.evaluations,
        &preexisting_quality_failure_names,
    );
    let mut worst_confidence = outcome.worst_confidence;
    let mut worst_merge = outcome.worst_merge;
    let mut blocking_issues = outcome.blocking_issues;
    let mut review_caveats = outcome.advisory_caveats;
    let mut gate_checks = Vec::new();

    let inline_findings_path =
        (inline.findings_count > 0).then_some("30_context/INLINE_FINDINGS.sarif");

    for (eval, effective_eval) in policy_summary
        .evaluations
        .iter()
        .zip(&outcome.effective_evals)
    {
        // Match the executed check by name, not by re-deriving an id: the policy
        // engine and the artifact writer spell a few ids differently (cargo
        // check→cargo, typescript→tsc, vitest→tests), so an id round-trip drops
        // the match and an executed check falls through to the "no artifact"
        // branch — reporting execution_state=executed with null evidence and log
        // (P4: an executed check must always carry its result artifact and log).
        let executed_check = checks.iter().find(|check| check.name == eval.name);
        // Evidence/log must reference the file the artifact writer actually
        // wrote, which is keyed by the artifact-side id, not the policy id.
        let artifact_id = executed_check.map(|check| check_id_from_name(&check.name));
        gate_checks.push(json!({
            "id": eval.check_id,
            "name": eval.name,
            "status": eval.raw_status,
            "execution_state": eval.execution_state,
            "outcome": eval.outcome,
            "class": gate_class_to_str(eval.gate_class),
            "severity": policy_severity_to_str(eval.severity),
            "policy_conclusion": effective_eval.conclusion,
            "confidence_impact": effective_eval.confidence_impact,
            "merge_impact": effective_eval.merge_impact,
            "blocking": matches!(effective_eval.merge_impact, MergeRecommendation::Block),
            // Skipped/unavailable checks have no executed CheckResult, so they
            // carry no measured duration and no result.json. Emit contract-valid
            // placeholders (non-negative duration, non-empty evidence) instead of
            // null, so MERGE_GATE.json passes its own validator on runners that
            // lack a tool (P1: artifact must not fail its own gate).
            "duration_secs": executed_check
                .map(|check| check.duration.as_secs_f32())
                .unwrap_or(0.0),
            "cached": executed_check.map(|check| check.cached),
            "reason": effective_eval.reason,
            "evidence": match &artifact_id {
                Some(id) => format!("20_quality/{}.result.json", id),
                None => eval
                    .reason
                    .clone()
                    .filter(|reason| !reason.trim().is_empty())
                    .unwrap_or_else(|| "skipped — no artifact generated".to_string()),
            },
            "log": artifact_id
                .as_ref()
                .map(|id| format!("20_quality/{}.log", id)),
        }));
    }

    // Only add heuristics gate check if not already present via synthetic check in all_checks
    let has_heuristics = checks
        .iter()
        .any(|c| check_id_from_name(&c.name) == "heuristics_loctree");
    if !has_heuristics {
        let (heuristics_check, heuristics_issue) = build_heuristics_gate_check(config, heuristics);
        if let Some(issue) = heuristics_issue {
            blocking_issues.push(issue);
            worst_merge = MergeRecommendation::Block;
        }
        gate_checks.push(heuristics_check);
    }

    let inline_severity = config.policy.severity_for("inline_findings");
    // THREAD 7: gate on introduced/unclassified findings, not the raw error
    // count — a scan with only pre-existing errors must not block the merge.
    let inline_class = effective_inline_gate_class(inline, &clean_comparison);
    let inline_blocking = config.policy.is_blocking(inline_severity, inline_class);
    if inline_blocking {
        blocking_issues.push(format!("INLINE_FINDINGS ({})", inline.status));
        worst_merge = MergeRecommendation::Block;
    }

    let policy_allow_merge = blocking_issues.is_empty();

    let quality_pass = !quality_failures.has_new_failures();

    if !quality_pass && worst_merge == MergeRecommendation::Approve {
        worst_merge = MergeRecommendation::ReviewRequired;
    }
    if !quality_pass && worst_confidence == AnalysisStatus::Complete {
        worst_confidence = AnalysisStatus::Degraded;
    }

    if !diffs.is_empty() {
        let risk_scores = signal::compute_file_risk_scores_with_root(
            diffs,
            coverage,
            breaking,
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
    }
    // Derive the scalar decision fields from the FINAL axes (after every
    // review/risk bump above) through the single coherent source. `allow_merge`
    // is owned here and never set independently, so it cannot contradict the
    // verdict (PV-03: no `allow_merge:true` beside a CONDITIONAL/BLOCK verdict).
    let decision_fields = derive_decision(worst_confidence, worst_merge, quality_pass);
    let allow_merge = decision_fields.allow_merge;
    let legacy_recommended_merge = decision_fields.recommended_merge;

    let mut all_review_caveats = build_review_caveats(breaking, coverage, inline.findings_count);
    all_review_caveats.extend(review_caveats);
    all_review_caveats.extend(rust_quality_review_caveats(config, checks));
    all_review_caveats.extend(cargo_audit_review_caveats(checks));
    all_review_caveats.extend(skipped_requested_security_review_caveats(
        config,
        checks,
        skipped_checks,
    ));
    if !quality_failures.preexisting_quality_failures.is_empty() {
        let names = quality_failures.preexisting_quality_failures.join(", ");
        all_review_caveats.push(format!(
            "Pre-existing quality failures (not from this diff): {}",
            names
        ));
    }

    if worst_merge == MergeRecommendation::ReviewRequired && all_review_caveats.is_empty() {
        all_review_caveats.push("Partial or degraded analysis coverage".to_string());
    }

    let decision = build_merge_decision_view(
        policy_allow_merge,
        quality_pass,
        legacy_recommended_merge,
        &quality_failures.quality_failures,
        &quality_failures.details,
        &blocking_issues,
        all_review_caveats.clone(),
    );

    // Split the inline findings the PR actually introduced from pre-existing
    // whole-repo debt, so the gate headline does not over-state the PR's
    // contribution. Derived from per-finding `in_diff`. NOTE: these count only
    // tool-finding rows (dashboard_findings) and exclude cargo-audit / check
    // SARIF rows, so introduced + preexisting may be < findings_count.
    let introduced_inline = inline
        .dashboard_findings
        .iter()
        .filter(|f| f.in_diff == Some(true))
        .count();
    let preexisting_inline = inline
        .dashboard_findings
        .iter()
        .filter(|f| f.in_diff == Some(false))
        .count();

    let gate = json!({
        "schema_version": "2.1",
        "generated_at": chrono::Local::now().to_rfc3339(),
        "bridge_stage": config.bridge_stage,
        "target": resolved_target.name,
        "bases": resolved_bases.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
        "profile": config.profile.kind.as_str(),
        "policy": {
            "version": config.policy.version,
            "mode": config.policy.mode_str(),
            "default_severity": policy_severity_to_str(config.policy.default_severity),
            "source": config.policy_file.display().to_string()
        },
        "checks": &gate_checks,
        "inline_findings": {
            "file": inline_findings_path,
            "file_exists": inline.findings_count > 0,
            "status": inline.status,
            "severity": policy_severity_to_str(inline_severity),
            "blocking": inline_blocking,
            "findings_count": inline.findings_count,
            "introduced_count": introduced_inline,
            "preexisting_count": preexisting_inline
        },
        "decision": {
            "analysis_status": worst_confidence,
            "merge_recommendation": worst_merge,
            "verdict": decision_fields.verdict,
            "allow_merge": allow_merge,
            "policy_allow_merge": policy_allow_merge,
            "quality_pass": quality_pass,
            "recommended_merge": legacy_recommended_merge,
            "recommended_label": decision.state.gate_label(),
            "quality_failures": quality_failures.quality_failures,
            "introduced_quality_failures": quality_failures.introduced_quality_failures,
            "preexisting_quality_failures": quality_failures.preexisting_quality_failures,
            "mixed_quality_failures": quality_failures.mixed_quality_failures,
            "unclassified_quality_failures": quality_failures.unclassified_quality_failures,
            "quality_failure_details": quality_failures.details.iter().map(|detail| json!({
                "name": detail.name,
                "classification": detail.classification.as_str(),
            })).collect::<Vec<_>>(),
            "decision_reason": decision.reason,
            "review_caveats": all_review_caveats,
            "blocking_issues": blocking_issues
        },
        "files": {
            "merge_gate_json": "00_summary/MERGE_GATE.json",
            "inline_findings": inline_findings_path,
            "full_patch": "10_diff/full.patch",
            "checks_log": "20_quality/full-checks.log",
            "dashboard": "dashboard.html"
        }
    });

    fs::write(
        dir.join("MERGE_GATE.json"),
        serde_json::to_string_pretty(&gate)?,
    )?;

    let mut md = String::new();
    md.push_str("# Merge Gate\n\n");
    md.push_str(&format!(
        "- Generated: {}\n- Policy mode: `{}`\n\n",
        chrono::Local::now().to_rfc3339(),
        config.policy.mode_str(),
    ));
    md.push_str(&format!(
        "- Verdict: `{}`\n- Recommended label: `{}`\n- Reason: {}\n\n",
        decision_fields.verdict,
        decision.state.gate_label(),
        decision.reason,
    ));
    md.push_str("## Checks\n\n");
    md.push_str("| Check | Status | Class | Blocking |\n");
    md.push_str("|---|---|---|---|\n");
    for check in &gate_checks {
        let _ = writeln!(
            md,
            "| {} | `{}` | `{}` | `{}` |",
            check["name"].as_str().unwrap_or("unknown"),
            check["status"].as_str().unwrap_or("unknown"),
            check["class"].as_str().unwrap_or("unknown"),
            check["blocking"].as_bool().unwrap_or(false),
        );
    }
    fs::write(dir.join("MERGE_GATE.md"), md)?;
    Ok(())
}

/// Merge-gate axes and issue lists after the pre-existing downgrade has been
/// applied to every evaluation. Shared verbatim by the merge gate and the
/// dashboard context so the two artifacts can never disagree on the verdict: a
/// pre-existing-only blocked check downgraded in one path but not the other
/// used to yield `MERGE_GATE=PASS` beside `report.json=CONDITIONAL/BLOCK`.
pub(super) struct EffectivePolicyOutcome {
    pub worst_confidence: crate::policy::engine::AnalysisStatus,
    pub worst_merge: crate::policy::engine::MergeRecommendation,
    pub blocking_issues: Vec<String>,
    pub advisory_caveats: Vec<String>,
    /// Per-evaluation effective view, index-aligned with the input `evaluations`.
    pub effective_evals: Vec<crate::policy::engine::CheckEvaluation>,
}

/// Compute the effective merge-gate outcome from the raw policy evaluations plus
/// the set of checks whose failures are purely pre-existing (all findings
/// outside the diff). Pre-existing-only checks are downgraded to advisory/approve
/// and excluded from the blocking axes; every other check bumps the axes as
/// normal. This is the single source of truth for THREAD 5's verdict parity.
pub(super) fn compute_effective_policy_outcome(
    evaluations: &[crate::policy::engine::CheckEvaluation],
    preexisting_quality_failure_names: &std::collections::BTreeSet<&str>,
) -> EffectivePolicyOutcome {
    use crate::policy::engine::{AnalysisStatus, MergeRecommendation, PolicyConclusion};

    let mut worst_confidence = AnalysisStatus::Complete;
    let mut worst_merge = MergeRecommendation::Approve;
    let mut blocking_issues = Vec::new();
    let mut advisory_caveats = Vec::new();
    let mut effective_evals = Vec::with_capacity(evaluations.len());

    for eval in evaluations {
        let preexisting_only = preexisting_quality_failure_names.contains(eval.name.as_str());
        let effective_eval = effective_quality_gate_eval(eval, preexisting_only);
        // The confidence axis bumps for EVERY check, including pre-existing-only
        // ones. The downgrade only neutralises the finding/merge impact — it must
        // not launder a degraded/incomplete analysis into Complete (R5-24). Since
        // a downgraded eval carries merge_impact = Approve, bumping the merge axis
        // here is a no-op for it, so only its (preserved) confidence propagates.
        bump_effective_gate_axes(&mut worst_confidence, &mut worst_merge, &effective_eval);
        if !preexisting_only {
            if effective_eval.conclusion == PolicyConclusion::Blocked {
                blocking_issues.push(format!(
                    "{} ({})",
                    eval.name,
                    display_raw_status(&eval.raw_status)
                ));
            } else if effective_eval.conclusion == PolicyConclusion::Advisory {
                advisory_caveats.push(describe_policy_advisory(eval));
            }
        }
        effective_evals.push(effective_eval);
    }

    EffectivePolicyOutcome {
        worst_confidence,
        worst_merge,
        blocking_issues,
        advisory_caveats,
        effective_evals,
    }
}

fn effective_quality_gate_eval(
    eval: &crate::policy::engine::CheckEvaluation,
    preexisting_only: bool,
) -> crate::policy::engine::CheckEvaluation {
    if !preexisting_only {
        return eval.clone();
    }

    let mut effective = eval.clone();
    effective.conclusion = crate::policy::engine::PolicyConclusion::Advisory;
    // Only the finding-derived impact is downgraded. `confidence_impact` is the
    // orthogonal analysis-completeness axis and is preserved verbatim: a
    // pre-existing-only check whose scan was degraded/incomplete (e.g. a semgrep
    // partial parse) must keep that signal so the verdict cannot become a clean
    // PASS on a scan that never analysed the whole target (R5-24).
    effective.merge_impact = crate::policy::engine::MergeRecommendation::Approve;
    effective.reason = Some("pre-existing findings outside the change".to_string());
    effective
}

fn bump_effective_gate_axes(
    analysis_status: &mut crate::policy::engine::AnalysisStatus,
    merge_recommendation: &mut crate::policy::engine::MergeRecommendation,
    eval: &crate::policy::engine::CheckEvaluation,
) {
    use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

    if eval.confidence_impact == AnalysisStatus::Incomplete {
        *analysis_status = AnalysisStatus::Incomplete;
    } else if eval.confidence_impact == AnalysisStatus::Degraded
        && *analysis_status == AnalysisStatus::Complete
    {
        *analysis_status = AnalysisStatus::Degraded;
    }

    if eval.merge_impact == MergeRecommendation::Block {
        *merge_recommendation = MergeRecommendation::Block;
    } else if eval.merge_impact == MergeRecommendation::ReviewRequired
        && *merge_recommendation == MergeRecommendation::Approve
    {
        *merge_recommendation = MergeRecommendation::ReviewRequired;
    }
}

fn describe_policy_advisory(eval: &crate::policy::engine::CheckEvaluation) -> String {
    use crate::policy::engine::CheckExecutionState;

    if eval.raw_status == "skipped" {
        if let Some(reason) = &eval.reason {
            return format!("{} skipped: {}", eval.name, reason);
        }
        return format!("{} was skipped", eval.name);
    }

    match eval.execution_state {
        CheckExecutionState::Executed => format!("{} returned {}", eval.name, eval.raw_status),
        CheckExecutionState::Skipped => format!("{} was skipped", eval.name),
        CheckExecutionState::Unavailable => format!("{} was unavailable for this run", eval.name),
        CheckExecutionState::Unknown => format!("{} needs manual review", eval.name),
    }
}

fn display_raw_status(status: &str) -> String {
    let mut chars = status.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{CheckResult, CheckStatus};
    use crate::config::test_config;
    use crate::git::ResolvedRef;
    use std::time::Duration;

    fn semgrep_check() -> CheckResult {
        CheckResult {
            name: "Semgrep scan".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_millis(25),
            output: "{}".to_string(),
            cached: false,
            provenance: None,
        }
    }

    fn semgrep_dashboard_finding(path: &str, in_diff: bool) -> DashboardFinding {
        DashboardFinding {
            level: "error",
            check_name: "Semgrep scan".to_string(),
            check_id: "semgrep_scan".to_string(),
            message: format!("finding in {path}"),
            in_diff: Some(in_diff),
        }
    }

    fn empty_coverage() -> CoverageDelta {
        CoverageDelta {
            total_source: 0,
            covered_count: 0,
            pct: 0,
            uncovered: Vec::new(),
            covered: Vec::new(),
            non_code_count: 0,
            ghost_tests: Vec::new(),
        }
    }

    fn resolved_refs() -> (ResolvedRef, Vec<ResolvedRef>) {
        (
            ResolvedRef {
                name: "feature".to_string(),
                commit_id: "2222222222222222222222222222222222222222".to_string(),
                is_remote: false,
            },
            vec![ResolvedRef {
                name: "main".to_string(),
                commit_id: "1111111111111111111111111111111111111111".to_string(),
                is_remote: false,
            }],
        )
    }

    fn run_gate_with_semgrep_finding(in_diff: bool, security_full: bool) -> serde_json::Value {
        run_gate_with_semgrep_finding_scan(in_diff, security_full, true)
    }

    fn run_gate_with_semgrep_finding_scan(
        in_diff: bool,
        security_full: bool,
        clean_comparison: bool,
    ) -> serde_json::Value {
        // The gate tests exercise a local checkout target, so map the historical
        // global `clean_comparison` bool onto the worktree-clean axis.
        let clean_comparison = CleanComparison::for_test(true, clean_comparison);
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = test_config();
        config.security_full = security_full;
        let checks = vec![semgrep_check()];
        let inline = InlineFindingsSummary {
            status: "failed".to_string(),
            findings_count: 1,
            dashboard_findings: vec![semgrep_dashboard_finding(
                if in_diff { "src/b.rs" } else { "src/a.rs" },
                in_diff,
            )],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison,
        })
        .expect("merge gate");

        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        serde_json::from_str(&raw).expect("parse gate json")
    }

    fn run_gate_with_semgrep_output(output: &str, in_diff: bool) -> serde_json::Value {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let checks = vec![CheckResult {
            name: "Semgrep scan".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_millis(25),
            output: output.to_string(),
            cached: false,
            provenance: None,
        }];
        let inline = InlineFindingsSummary {
            status: "failed".to_string(),
            findings_count: 1,
            dashboard_findings: vec![semgrep_dashboard_finding(
                if in_diff { "src/b.rs" } else { "src/a.rs" },
                in_diff,
            )],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
        .expect("merge gate");

        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        serde_json::from_str(&raw).expect("parse gate json")
    }

    fn cargo_test_check() -> CheckResult {
        CheckResult {
            name: "cargo test".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_millis(25),
            output: "{}".to_string(),
            cached: false,
            provenance: None,
        }
    }

    fn run_gate_with_cargo_test_finding(in_diff: bool) -> serde_json::Value {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let checks = vec![cargo_test_check()];
        let inline = InlineFindingsSummary {
            status: "failed".to_string(),
            findings_count: 1,
            dashboard_findings: vec![DashboardFinding {
                level: "error",
                check_name: "Cargo Test".to_string(),
                check_id: "cargo_test".to_string(),
                message: "test failed".to_string(),
                in_diff: Some(in_diff),
            }],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
        .expect("merge gate");

        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        serde_json::from_str(&raw).expect("parse gate json")
    }

    #[test]
    fn effective_outcome_is_the_single_shared_verdict_source() {
        use crate::policy::engine::{MergeRecommendation, PolicyEngine};

        // THREAD 5: both the merge gate and the dashboard context feed the same
        // evaluations through this one function, so their verdicts cannot drift.
        let config = test_config();
        let engine = PolicyEngine::new(&config);
        let checks = vec![semgrep_check()];
        let summary = engine.evaluate_all(&checks, &[]);

        // Pre-existing-only: the failing check is downgraded off the axes.
        let mut preexisting = std::collections::BTreeSet::new();
        preexisting.insert("Semgrep scan");
        let downgraded = compute_effective_policy_outcome(&summary.evaluations, &preexisting);
        assert!(downgraded.blocking_issues.is_empty());
        assert!(downgraded.advisory_caveats.is_empty());
        assert_eq!(downgraded.worst_merge, MergeRecommendation::Approve);

        // Not pre-existing: the same failure keeps its policy impact.
        let kept = compute_effective_policy_outcome(
            &summary.evaluations,
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(kept.worst_merge, MergeRecommendation::ReviewRequired);
        assert_eq!(kept.advisory_caveats.len(), 1);
    }

    #[test]
    fn failed_cargo_test_outside_diff_does_not_get_pass() {
        // THREAD 4: a whole-project gate failing with an out-of-diff location
        // must NOT be downgraded to pre-existing — the diff may have caused it.
        let gate = run_gate_with_cargo_test_finding(false);

        assert_ne!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(false));
        assert!(
            gate["decision"]["preexisting_quality_failures"]
                .as_array()
                .is_none_or(|arr| arr.is_empty()),
            "cargo test must not land in the pre-existing bucket"
        );
        assert_eq!(
            gate["decision"]["unclassified_quality_failures"][0].as_str(),
            Some("cargo test")
        );
    }

    #[test]
    fn preexisting_semgrep_finding_outside_diff_does_not_degrade_verdict() {
        let gate = run_gate_with_semgrep_finding(false, false);

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(true));
        assert_eq!(
            gate["decision"]["preexisting_quality_failures"][0].as_str(),
            Some("Semgrep scan")
        );
        assert_eq!(
            gate["checks"][0]["reason"].as_str(),
            Some("pre-existing findings outside the change")
        );
        assert_eq!(gate["checks"][0]["blocking"].as_bool(), Some(false));
    }

    #[test]
    fn preexisting_semgrep_with_scan_errors_does_not_get_clean_pass() {
        // R5-24: a full scan whose findings all sit out-of-diff is downgraded off
        // the finding axis, but its errors[] mean part of the target was never
        // parsed. The degraded-analysis signal must survive the downgrade so the
        // verdict is CONDITIONAL, not a clean PASS that hides the partial
        // coverage — an introduced finding could hide in the unparsed spans.
        let gate = run_gate_with_semgrep_output(
            r#"{"results":[],"errors":[{"type":["PartialParsing",[]],"level":"warn"}]}"#,
            false,
        );

        assert_ne!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(
            gate["decision"]["analysis_status"].as_str(),
            Some("degraded")
        );
        // The finding impact is still downgraded: it lands in the pre-existing
        // bucket, not as a new failure that blocks.
        assert_eq!(
            gate["decision"]["preexisting_quality_failures"][0].as_str(),
            Some("Semgrep scan")
        );
    }

    #[test]
    fn downgrade_of_degraded_scan_keeps_confidence_but_drops_finding_impact() {
        use crate::policy::engine::{AnalysisStatus, MergeRecommendation, PolicyEngine};

        // R5-24 at the shared-outcome level: the engine degrades a partial
        // semgrep scan's confidence, and the pre-existing downgrade preserves it
        // while neutralising the finding/merge impact.
        let config = test_config();
        let engine = PolicyEngine::new(&config);
        let degraded = CheckResult {
            name: "Semgrep scan".to_string(),
            status: CheckStatus::Failed,
            duration: Duration::from_millis(1),
            output: r#"{"results":[],"errors":[{"type":["PartialParsing",[]]}]}"#.to_string(),
            cached: false,
            provenance: None,
        };
        let summary = engine.evaluate_all(std::slice::from_ref(&degraded), &[]);
        assert_eq!(
            summary.evaluations[0].confidence_impact,
            AnalysisStatus::Degraded,
            "a partial semgrep scan degrades the analysis confidence"
        );

        let mut preexisting = std::collections::BTreeSet::new();
        preexisting.insert("Semgrep scan");
        let outcome = compute_effective_policy_outcome(&summary.evaluations, &preexisting);
        assert_eq!(outcome.worst_merge, MergeRecommendation::Approve);
        assert!(outcome.blocking_issues.is_empty());
        assert_eq!(
            outcome.worst_confidence,
            AnalysisStatus::Degraded,
            "the downgrade must not launder a degraded scan back to Complete"
        );
    }

    #[test]
    fn introduced_semgrep_finding_in_diff_degrades_verdict() {
        let gate = run_gate_with_semgrep_finding(true, false);

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("CONDITIONAL"));
        assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(false));
        assert_eq!(
            gate["decision"]["introduced_quality_failures"][0].as_str(),
            Some("Semgrep scan")
        );
    }

    #[test]
    fn security_full_preexisting_semgrep_finding_is_advisory_only() {
        let gate = run_gate_with_semgrep_finding(false, true);

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(true));
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .expect("review caveats")
                .iter()
                .any(|caveat| caveat
                    .as_str()
                    .is_some_and(|value| value.contains("Pre-existing quality failures")))
        );
    }

    fn rustfmt_warnings_check() -> CheckResult {
        CheckResult {
            name: "Rustfmt".to_string(),
            status: CheckStatus::Warnings,
            duration: Duration::from_millis(25),
            output: "Diff in src/a.rs".to_string(),
            cached: false,
            provenance: None,
        }
    }

    fn run_gate_with_rustfmt_warning(in_diff: bool) -> serde_json::Value {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let checks = vec![rustfmt_warnings_check()];
        let inline = InlineFindingsSummary {
            status: "warnings".to_string(),
            findings_count: 1,
            dashboard_findings: vec![DashboardFinding {
                level: "warning",
                check_name: "Rustfmt".to_string(),
                check_id: "rustfmt".to_string(),
                message: "needs formatting".to_string(),
                in_diff: Some(in_diff),
            }],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
        .expect("merge gate");

        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        serde_json::from_str(&raw).expect("parse gate json")
    }

    #[test]
    fn preexisting_rustfmt_warning_out_of_diff_is_pass_with_caveat() {
        // R2-13: a warning-level baseline-signal check (Rustfmt) whose findings
        // all sit outside the diff is pre-existing debt and must get the same
        // downgrade as a failure — PASS with a pre-existing caveat, not
        // CONDITIONAL.
        let gate = run_gate_with_rustfmt_warning(false);

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(true));
        assert_eq!(
            gate["decision"]["preexisting_quality_failures"][0].as_str(),
            Some("Rustfmt")
        );
    }

    #[test]
    fn introduced_rustfmt_warning_in_diff_is_not_downgraded() {
        // In-diff formatting warnings belong to the change: no downgrade.
        let gate = run_gate_with_rustfmt_warning(true);

        assert_ne!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(
            gate["decision"]["introduced_quality_failures"][0].as_str(),
            Some("Rustfmt")
        );
    }

    #[test]
    fn dirty_scan_out_of_diff_semgrep_finding_does_not_pass() {
        // R2-9: the same out-of-diff semgrep finding that is downgraded to
        // pre-existing on a clean scan must NOT be downgraded when the scan
        // analysed a dirty working tree — it could be an uncommitted change.
        let gate = run_gate_with_semgrep_finding_scan(false, false, false);

        assert_ne!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(false));
        assert!(
            gate["decision"]["preexisting_quality_failures"]
                .as_array()
                .is_none_or(|arr| arr.is_empty()),
            "a dirty-scan out-of-diff finding must not land in the pre-existing bucket"
        );
        assert_eq!(
            gate["decision"]["unclassified_quality_failures"][0].as_str(),
            Some("Semgrep scan")
        );
    }
}
