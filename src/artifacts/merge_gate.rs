//! MERGE_GATE.md generation via the policy engine.

use super::*;

pub(super) fn generate_merge_gate(input: MergeGateInput<'_>) -> Result<()> {
    use crate::policy::engine::{
        AnalysisStatus, MergeRecommendation, PolicyConclusion, PolicyEngine,
    };
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
    } = input;

    let engine = PolicyEngine::new(config);
    let policy_summary = engine.evaluate_all(checks, skipped_checks);
    let quality_failures = build_quality_failure_summary(checks, &inline.dashboard_findings);
    let preexisting_quality_failure_names = quality_failures
        .preexisting_quality_failures
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut gate_checks = Vec::new();
    let mut blocking_issues = Vec::new();
    let mut review_caveats = Vec::new();
    let mut worst_confidence = AnalysisStatus::Complete;
    let mut worst_merge = MergeRecommendation::Approve;

    let inline_findings_path =
        (inline.findings_count > 0).then_some("30_context/INLINE_FINDINGS.sarif");

    for eval in &policy_summary.evaluations {
        let preexisting_only = preexisting_quality_failure_names.contains(eval.name.as_str());
        let effective_eval = effective_quality_gate_eval(eval, preexisting_only);
        if !preexisting_only {
            bump_effective_gate_axes(&mut worst_confidence, &mut worst_merge, &effective_eval);
            if effective_eval.conclusion == PolicyConclusion::Blocked {
                blocking_issues.push(format!(
                    "{} ({})",
                    eval.name,
                    display_raw_status(&eval.raw_status)
                ));
            } else if effective_eval.conclusion == PolicyConclusion::Advisory {
                review_caveats.push(describe_policy_advisory(eval));
            }
        }

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
    let inline_class = if inline.status == "failed" {
        GateClass::Fail
    } else if inline.status == "warnings" {
        GateClass::Info
    } else {
        GateClass::Pass
    };
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

fn effective_quality_gate_eval(
    eval: &crate::policy::engine::CheckEvaluation,
    preexisting_only: bool,
) -> crate::policy::engine::CheckEvaluation {
    if !preexisting_only {
        return eval.clone();
    }

    let mut effective = eval.clone();
    effective.conclusion = crate::policy::engine::PolicyConclusion::Advisory;
    effective.confidence_impact = crate::policy::engine::AnalysisStatus::Complete;
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
        })
        .expect("merge gate");

        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        serde_json::from_str(&raw).expect("parse gate json")
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
}
