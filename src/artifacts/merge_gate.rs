//! MERGE_GATE.md generation via the policy engine.

use super::*;

/// How old a replayed result may be before a verdict resting on it earns an
/// advisory caveat.
///
/// Seven days is deliberately loose. The caveat exists for the shape seen in the
/// Vista dogfood run (`PRV-CACHE-STALENESS`): a `Cargo audit` result replayed
/// from a cache written before a reboot co-authored a `BLOCK`, and nothing in
/// the pack said the evidence was days old. The same omission on a cached PASS
/// can support a clean verdict after the toolchain changes. A tight threshold
/// would annotate ordinary same-day replays and teach readers to ignore the
/// field, so the bar is set where "this evidence may simply be out of date" is
/// the honest reading.
///
/// The caveat is WARN-ONLY: it is an additive report about the pack and changes
/// no verdict, no exit code, and no other field. The threshold is a constant on
/// purpose — making it configurable is a follow-up, not part of stating the
/// fact.
const STALE_CACHE_CAVEAT_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// The age of the stored result `check_name` was replayed from, when this run
/// replayed one and the ledger recorded how old it was.
///
/// Only [`TaskKind::Check`] entries answer. A context artifact backed by the
/// same tool is different work under the same id, and its replay says nothing
/// about the gate row this caveat is about.
fn replayed_cache_age_secs(ledger: &crate::ledger::TaskLedger, check_name: &str) -> Option<u64> {
    use crate::ledger::{TaskKind, TaskState};

    let tool = crate::check_id::check_id_from_name(check_name);
    let entries = ledger.entries();
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == TaskKind::Check && entry.key.tool == tool)
        .and_then(|entry| match &entry.state {
            TaskState::Cached { cache_age_secs, .. } => *cache_age_secs,
            _ => None,
        })
}

pub(super) fn generate_merge_gate(input: MergeGateInput<'_>) -> Result<()> {
    use crate::policy::engine::{
        AnalysisStatus, EnforcementDisposition, MergeRecommendation, PolicyEngine,
    };
    use serde_json::json;
    use std::collections::BTreeSet;
    let MergeGateInput {
        dir,
        config,
        ledger,
        checks,
        heuristics,
        inline,
        breaking,
        rust_api_delta,
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
    let mut enforcement_disposition =
        EnforcementDisposition::from_evaluations(&outcome.effective_evals);
    let mut gate_checks = Vec::new();
    let mut stale_cache_caveats = Vec::new();

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

        // A verdict may rest on evidence this run never produced. That is true
        // for a stale failure holding the merge AND for a stale pass allowing a
        // clean decision after the compiler/toolchain changed. Name every old
        // replayed gate row; the ledger lookup already proves this exact check
        // came from cache, while the caveat remains advisory-only.
        if let Some(age) = replayed_cache_age_secs(ledger, &eval.name)
            && age > STALE_CACHE_CAVEAT_MAX_AGE_SECS
        {
            stale_cache_caveats.push(json!({
                "check_id": eval.check_id,
                "check_name": eval.name,
                "cache_age_secs": age,
                "threshold_secs": STALE_CACHE_CAVEAT_MAX_AGE_SECS,
            }));
        }
    }

    // Only add heuristics gate check if not already present via synthetic check in all_checks
    let has_heuristics = checks
        .iter()
        .any(|c| check_id_from_name(&c.name) == "heuristics_loctree");
    if !has_heuristics {
        let (heuristics_check, heuristics_issue, heuristics_disposition) =
            build_heuristics_gate_check(config, heuristics);
        enforcement_disposition.raise_to(heuristics_disposition);
        if let Some(issue) = heuristics_issue {
            record_blocking_issue(&mut blocking_issues, &mut worst_merge, issue);
            enforcement_disposition.raise_to(EnforcementDisposition::Block);
        }
        gate_checks.push(heuristics_check);
    }

    // THREAD 7: gate on introduced/unclassified findings, not the raw error
    // count — a scan with only pre-existing errors must not block the merge.
    let inline_gate = apply_inline_gate_outcome(
        config,
        inline,
        &clean_comparison,
        &mut blocking_issues,
        &mut worst_merge,
    );
    let inline_severity = inline_gate.severity;
    let inline_blocking = inline_gate.blocking;
    let inline_enforcement_disposition = if inline_gate.blocking {
        EnforcementDisposition::Block
    } else {
        match inline_gate.class {
            crate::policy::GateClass::Fail => EnforcementDisposition::ReviewRequired,
            crate::policy::GateClass::Info => EnforcementDisposition::WarningsOnly,
            crate::policy::GateClass::Pass | crate::policy::GateClass::Skip
                if inline.status.eq_ignore_ascii_case("warnings") =>
            {
                EnforcementDisposition::WarningsOnly
            }
            crate::policy::GateClass::Pass | crate::policy::GateClass::Skip => {
                EnforcementDisposition::Clean
            }
        }
    };
    enforcement_disposition.raise_to(inline_enforcement_disposition);

    let policy_allow_merge = blocking_issues.is_empty();

    let quality_pass = !quality_failures.has_new_failures();

    if !quality_pass && worst_merge == MergeRecommendation::Approve {
        worst_merge = MergeRecommendation::ReviewRequired;
    }
    if !quality_pass && worst_confidence == AnalysisStatus::Complete {
        worst_confidence = AnalysisStatus::Degraded;
    }
    if !quality_pass {
        enforcement_disposition.raise_to(EnforcementDisposition::ReviewRequired);
    }

    if !diffs.is_empty() {
        let risk_scores = signal::compute_file_risk_scores_with_api(
            diffs,
            coverage,
            breaking,
            rust_api_delta,
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
            enforcement_disposition.raise_to(EnforcementDisposition::ReviewRequired);
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
            enforcement_disposition.raise_to(EnforcementDisposition::ReviewRequired);
        }
    }
    // Breaking-change escalation (critic-1): a genuine breaking API change must
    // raise the verdict to at least CONDITIONAL. Identical bump to the one in
    // `build_dashboard_context`, so MERGE_GATE.json and report.json can never
    // disagree on the verdict. Gated by the `breaking_escalation` knob; when off
    // the breaking findings stay visible as an informational caveat only.
    if let Some(reason) =
        apply_breaking_escalation(config.breaking_escalation, breaking, &mut worst_merge)
    {
        enforcement_disposition.raise_to(EnforcementDisposition::ReviewRequired);
        review_caveats.push(reason);
    }
    enforcement_disposition.raise_to(apply_rust_api_delta_outcome(
        config.breaking_escalation,
        rust_api_delta,
        &mut worst_confidence,
        &mut worst_merge,
    ));

    // Fail-honest backstop for any typed ratchet added above without an
    // explicit disposition update. The warnings-only exception remains
    // representable only when that exact typed disposition was already set.
    if worst_merge == MergeRecommendation::Block {
        enforcement_disposition.raise_to(EnforcementDisposition::Block);
    } else if worst_confidence != AnalysisStatus::Complete
        || !quality_pass
        || (worst_merge == MergeRecommendation::ReviewRequired
            && enforcement_disposition == EnforcementDisposition::Clean)
    {
        enforcement_disposition.raise_to(EnforcementDisposition::ReviewRequired);
    }

    // Derive the scalar decision fields from the FINAL axes (after every
    // review/risk bump above) through the single coherent source. `allow_merge`
    // is owned here and never set independently, so it cannot contradict the
    // verdict (PV-03: no `allow_merge:true` beside a CONDITIONAL/BLOCK verdict).
    let decision_fields = derive_decision(worst_confidence, worst_merge, quality_pass);
    let allow_merge = decision_fields.allow_merge;
    let legacy_recommended_merge = decision_fields.recommended_merge;

    let mut all_review_caveats = build_review_caveats(breaking, coverage, inline.findings_count);
    all_review_caveats.extend(rust_api_delta_review_caveats(rust_api_delta));
    all_review_caveats.extend(review_caveats);
    all_review_caveats.extend(rust_quality_review_caveats(config, checks));
    all_review_caveats.extend(cargo_audit_review_caveats(checks));
    all_review_caveats.extend(cargo_audit_baseline_review_caveats(inline));
    all_review_caveats.extend(semgrep_partial_parse_review_caveats(checks));
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
        .filter(|finding| is_operator_finding(finding))
        .filter(|f| f.in_diff == Some(true))
        .count();
    let preexisting_inline = inline
        .dashboard_findings
        .iter()
        .filter(|finding| is_operator_finding(finding))
        .filter(|f| f.in_diff == Some(false))
        .count();

    let gate = json!({
        "schema_version": crate::gate::MERGE_GATE_SCHEMA_VERSION,
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
            "effective_class": gate_class_to_str(inline_gate.class),
            "enforcement_disposition": inline_enforcement_disposition,
            "findings_count": inline.findings_count,
            "introduced_count": introduced_inline,
            "preexisting_count": preexisting_inline
        },
        "rust_api_delta": rust_api_delta,
        // Additive, advisory, and deliberately OUTSIDE `decision`: naming a
        // blocking row whose evidence was replayed from an old cache is a report
        // about the pack, not an axis of it. The decision object is closed by
        // contract, and every field in it ranks the verdict — this one must not.
        "stale_cache_caveats": stale_cache_caveats,
        "decision": {
            "enforcement_disposition": enforcement_disposition,
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
                "origin": detail.origin.as_str(),
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
        "- Verdict: `{}`\n- Enforcement disposition: `{}`\n- Recommended label: `{}`\n- Reason: {}\n\n",
        decision_fields.verdict,
        enforcement_disposition.as_str(),
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
    use crate::checks::{CheckResult, CheckStatus, SkippedCheck};
    use crate::config::test_config;
    use crate::git::ResolvedRef;
    use std::time::Duration;

    /// A ledger with nothing recorded: the shape of a run where no gate row was
    /// replayed, so none of them can be stale.
    fn empty_ledger() -> crate::ledger::TaskLedger {
        crate::ledger::TaskLedger::new()
    }

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
            pct: None,
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

    #[test]
    fn merge_gate_serializes_the_exact_shared_rust_api_view() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let inline = InlineFindingsSummary {
            status: "passed".to_owned(),
            findings_count: 0,
            dashboard_findings: Vec::new(),
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();
        let view = api_delta::ApiArtifactView {
            view: api_delta::ApiArtifactViewKind::BreakingChanges,
            analysis_source: api_delta::REPO_BACKED_RUST_API_SOURCE,
            base_revision: "git_tree:base".to_owned(),
            target_revision: "git_tree:target".to_owned(),
            counts: api_delta::ApiDeltaCounts {
                added: 0,
                removed: 0,
                changed: 0,
                relocated: 0,
                visibility_changed: 0,
                unknown: 0,
            },
            findings: Vec::new(),
        };
        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: Some(&view),
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
        .expect("merge gate");
        let gate: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join("MERGE_GATE.json")).unwrap())
                .unwrap();
        assert_eq!(gate["rust_api_delta"], serde_json::to_value(view).unwrap());
    }

    /// One gate run whose failing row was REPLAYED from a stored result of the
    /// given age — the `PRV-CACHE-STALENESS` shape, with the age as the only
    /// variable.
    fn run_gate_with_cached_semgrep_status(
        cache_age_secs: u64,
        status: CheckStatus,
    ) -> serde_json::Value {
        use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let mut check = semgrep_check();
        let passing = status == CheckStatus::Passed;
        check.status = status;
        check.cached = true;
        let checks = vec![check];
        let inline = InlineFindingsSummary {
            status: if passing { "passed" } else { "failed" }.to_string(),
            findings_count: usize::from(!passing),
            dashboard_findings: (!passing)
                .then(|| semgrep_dashboard_finding("src/b.rs", true))
                .into_iter()
                .collect(),
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();

        let ledger = TaskLedger::new();
        ledger.record(TaskEntry {
            key: TaskKey::new("Semgrep scan", SubstrateKey::default()),
            kind: TaskKind::Check,
            state: TaskState::Cached {
                cache_age_secs: Some(cache_age_secs),
                origin: SubstrateKey::default(),
            },
            queued_at: None,
            started_at: None,
        });

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &ledger,
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
        .expect("merge gate");

        serde_json::from_slice(&std::fs::read(tmp.path().join("MERGE_GATE.json")).unwrap()).unwrap()
    }

    fn run_gate_with_cached_semgrep(cache_age_secs: u64) -> serde_json::Value {
        run_gate_with_cached_semgrep_status(cache_age_secs, CheckStatus::Failed)
    }

    /// The Vista dogfood shape: the row holding the merge came out of a cache
    /// written a week ago, and until now the pack said only "cached: true".
    #[test]
    fn a_blocking_row_replayed_from_an_old_cache_is_named() {
        let gate = run_gate_with_cached_semgrep(STALE_CACHE_CAVEAT_MAX_AGE_SECS + 60);
        let caveats = gate["stale_cache_caveats"]
            .as_array()
            .expect("stale_cache_caveats is an array");

        assert_eq!(caveats.len(), 1, "one stale blocking row, one caveat");
        assert_eq!(caveats[0]["check_id"], "semgrep_scan");
        assert_eq!(caveats[0]["check_name"], "Semgrep scan");
        assert_eq!(
            caveats[0]["cache_age_secs"],
            STALE_CACHE_CAVEAT_MAX_AGE_SECS + 60
        );
        assert_eq!(
            caveats[0]["threshold_secs"],
            STALE_CACHE_CAVEAT_MAX_AGE_SECS
        );
    }

    /// A clean decision can be just as stale as a block: the toolchain may have
    /// changed since a cached PASS was produced even when source inputs did not.
    #[test]
    fn a_passing_row_replayed_from_an_old_cache_is_named() {
        let gate = run_gate_with_cached_semgrep_status(
            STALE_CACHE_CAVEAT_MAX_AGE_SECS + 60,
            CheckStatus::Passed,
        );
        let caveats = gate["stale_cache_caveats"]
            .as_array()
            .expect("stale_cache_caveats is an array");

        assert_eq!(gate["checks"][0]["status"], "passed");
        assert_eq!(caveats.len(), 1, "one stale passing row, one caveat");
        assert_eq!(caveats[0]["check_id"], "semgrep_scan");
        assert_eq!(
            caveats[0]["cache_age_secs"],
            STALE_CACHE_CAVEAT_MAX_AGE_SECS + 60
        );
    }

    /// The caveat is advisory in the strong sense: the ONLY difference a stale
    /// replay makes to the pack is the additive field itself. A fresh replay of
    /// the same failing row raises nothing at all.
    #[test]
    fn the_stale_cache_caveat_moves_no_other_field() {
        let fresh = run_gate_with_cached_semgrep(60);
        assert!(
            fresh["stale_cache_caveats"]
                .as_array()
                .expect("stale_cache_caveats is an array")
                .is_empty(),
            "a minute-old replay is not stale evidence"
        );

        let stale = run_gate_with_cached_semgrep(STALE_CACHE_CAVEAT_MAX_AGE_SECS + 60);
        assert_eq!(
            stale["decision"], fresh["decision"],
            "the caveat must not move the verdict or any decision field"
        );
        assert_eq!(stale["checks"], fresh["checks"]);
        assert_eq!(stale["inline_findings"], fresh["inline_findings"]);
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
            ledger: &empty_ledger(),
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
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
            ledger: &empty_ledger(),
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
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
            ledger: &empty_ledger(),
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
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

    fn run_gate_with_skipped_policy_check(
        check_id: &str,
        name: &str,
        reason: &str,
        severity: crate::policy::PolicySeverity,
    ) -> serde_json::Value {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = test_config();
        config.policy.checks.insert(check_id.to_string(), severity);
        let inline = InlineFindingsSummary {
            status: "passed".to_string(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();
        let skipped_checks = vec![SkippedCheck {
            id: check_id.to_string(),
            name: name.to_string(),
            reason: reason.to_string(),
        }];

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &skipped_checks,
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
        .expect("merge gate");

        let raw =
            std::fs::read_to_string(tmp.path().join("MERGE_GATE.json")).expect("read gate json");
        serde_json::from_str(&raw).expect("parse gate json")
    }

    fn find_gate_check<'a>(gate: &'a serde_json::Value, check_id: &str) -> &'a serde_json::Value {
        gate["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .find(|check| check["id"].as_str() == Some(check_id))
            .expect("gate check")
    }

    #[test]
    fn artifact_consistency_inline_blocking_verdict_matches_report_and_gate() {
        use crate::policy::engine::MergeRecommendation;

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = test_config();
        config.policy.checks.insert(
            "inline_findings".to_string(),
            crate::policy::PolicySeverity::Block,
        );
        let inline = InlineFindingsSummary {
            status: "failed".to_string(),
            findings_count: 1,
            dashboard_findings: vec![DashboardFinding {
                level: "error",
                check_name: "Semgrep scan".to_string(),
                check_id: "semgrep_scan".to_string(),
                message: "introduced finding".to_string(),
                in_diff: Some(true),
            }],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
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
        let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate json");
        let dashboard = build_dashboard_context(DashboardContextInput {
            config: &config,
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: Vec::new(),
            rust_api_delta: None,
            coverage,
            diff_dir: tmp.path(),
            skipped_checks: Vec::new(),
            out_dir: tmp.path(),
            diffs: &[],
            ownership_map: Vec::new(),
            clean_comparison: CleanComparison::for_test(true, true),
        });

        assert_eq!(
            gate["decision"]["merge_recommendation"].as_str(),
            Some("block")
        );
        assert_eq!(dashboard.merge_recommendation, MergeRecommendation::Block);
        assert_eq!(
            dashboard.verdict,
            gate["decision"]["verdict"].as_str().expect("gate verdict")
        );
    }

    #[test]
    fn breaking_escalation_verdict_matches_across_gate_and_dashboard() {
        // Parity: the breaking-change escalation must land identically on
        // MERGE_GATE.json (this path) and the dashboard context that backs
        // report.json + the console verdict.
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config();
        let inline = InlineFindingsSummary {
            status: "passed".to_string(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();
        let breaking = vec![breaking_removed_symbol()];

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: &breaking,
            rust_api_delta: None,
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
        let gate: serde_json::Value = serde_json::from_str(&raw).expect("parse gate json");

        let dashboard = build_dashboard_context(DashboardContextInput {
            config: &config,
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: breaking.clone(),
            rust_api_delta: None,
            coverage,
            diff_dir: tmp.path(),
            skipped_checks: Vec::new(),
            out_dir: tmp.path(),
            diffs: &[],
            ownership_map: Vec::new(),
            clean_comparison: CleanComparison::for_test(true, true),
        });

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("CONDITIONAL"));
        assert_eq!(
            dashboard.verdict,
            gate["decision"]["verdict"].as_str().expect("gate verdict")
        );
        assert!(
            dashboard
                .review_caveats
                .iter()
                .any(|caveat| caveat == "breaking API change detected: 1 finding"),
            "dashboard context must carry the same escalation reason caveat"
        );
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
    fn mode_skip_required_check_is_caveat_not_blocking_issue() {
        let gate = run_gate_with_skipped_policy_check(
            "cargo_audit",
            "Cargo audit",
            "security disabled",
            crate::policy::PolicySeverity::Block,
        );
        let check = find_gate_check(&gate, "cargo_audit");

        assert_eq!(check["status"].as_str(), Some("skipped"));
        assert_eq!(check["policy_conclusion"].as_str(), Some("advisory"));
        assert_eq!(check["confidence_impact"].as_str(), Some("incomplete"));
        assert_eq!(check["merge_impact"].as_str(), Some("review_required"));
        assert_eq!(check["blocking"].as_bool(), Some(false));
        assert_eq!(gate["decision"]["verdict"].as_str(), Some("CONDITIONAL"));
        assert_eq!(gate["decision"]["policy_allow_merge"].as_bool(), Some(true));
        assert!(
            gate["decision"]["blocking_issues"]
                .as_array()
                .is_some_and(|items| items.is_empty()),
            "mode-skip must not land in blocking_issues"
        );
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item
                    .as_str()
                    .is_some_and(|text| text == "Cargo audit skipped: security disabled"))),
            "mode-skip should remain visible as a review caveat"
        );
    }

    #[test]
    fn missing_required_tool_skip_remains_blocking_issue() {
        let gate = run_gate_with_skipped_policy_check(
            "cargo_audit",
            "Cargo audit",
            "tool not installed (cargo-audit is missing)",
            crate::policy::PolicySeverity::Block,
        );
        let check = find_gate_check(&gate, "cargo_audit");

        assert_eq!(check["policy_conclusion"].as_str(), Some("blocked"));
        assert_eq!(check["confidence_impact"].as_str(), Some("incomplete"));
        assert_eq!(check["merge_impact"].as_str(), Some("block"));
        assert_eq!(check["blocking"].as_bool(), Some(true));
        assert_eq!(gate["decision"]["verdict"].as_str(), Some("BLOCK"));
        assert_eq!(
            gate["decision"]["policy_allow_merge"].as_bool(),
            Some(false)
        );
        assert!(
            gate["decision"]["blocking_issues"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item
                    .as_str()
                    .is_some_and(|text| text == "Cargo audit (Skipped)"))),
            "tool-missing skip must keep today's blocking behavior"
        );
    }

    #[test]
    fn the_blocker_flag_is_the_blocker_list_written_twice() {
        // `tools/validate_merge_gate.py` certifies
        // `policy_allow_merge == blocking_issues.is_empty()` as an equivalence,
        // rejecting a pack that states one without the other. That is only sound
        // while the flag is derived from the list and from nothing else, as it is
        // above. Should the flag ever gain a second input, this pin fails here
        // — in the emitter that changed — instead of the validator silently
        // rejecting packs prview itself still writes.
        let packs = [
            run_gate_with_skipped_policy_check(
                "cargo_audit",
                "Cargo audit",
                "security disabled",
                crate::policy::PolicySeverity::Block,
            ),
            run_gate_with_skipped_policy_check(
                "cargo_audit",
                "Cargo audit",
                "tool not installed (cargo-audit is missing)",
                crate::policy::PolicySeverity::Block,
            ),
            run_gate_with_cargo_test_finding(false),
            run_gate_with_semgrep_finding(false, false),
        ];

        for gate in packs {
            let decision = &gate["decision"];
            let no_blockers = decision["blocking_issues"]
                .as_array()
                .expect("blocking_issues array")
                .is_empty();
            assert_eq!(
                decision["policy_allow_merge"].as_bool(),
                Some(no_blockers),
                "policy_allow_merge must mirror an empty blocking_issues: {decision}"
            );
        }
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
            r#"{"results":[],"errors":[{"type":["PartialParsing",[]],"level":"warn","path":"src/ffi.rs"}]}"#,
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
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .expect("review caveats")
                .iter()
                .any(|caveat| caveat
                    .as_str()
                    .is_some_and(|value| value.contains("src/ffi.rs"))),
            "the operator-facing decision must name the incompletely parsed file"
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
        let mut config = test_config();
        config.policy.default_severity = crate::policy::PolicySeverity::Ignore;
        config
            .policy
            .checks
            .insert("rustfmt".to_string(), crate::policy::PolicySeverity::Warn);
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
            ledger: &empty_ledger(),
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &[],
            rust_api_delta: None,
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
        assert_eq!(
            gate["decision"]["enforcement_disposition"].as_str(),
            Some("warnings_only")
        );
        assert_eq!(gate["decision"]["quality_pass"].as_bool(), Some(true));
        assert_eq!(
            gate["decision"]["preexisting_quality_failures"][0].as_str(),
            Some("Rustfmt")
        );
    }

    #[test]
    fn preexisting_only_rustfmt_keeps_strict_gate_exit_zero() {
        // Regression guard for the warning→failure cut: the gate exit contract is
        // unchanged. The pre-existing-only rustfmt pack is a PASS, and `prview
        // gate --strict` must still exit 0 on it — the same artifact the adapter
        // in `gate.rs` reads, run through the same verdict → exit mapping.
        use crate::gate::{GateVerdict, gate_exit_code};

        let gate = run_gate_with_rustfmt_warning(false);
        let verdict = GateVerdict::try_from(
            gate["decision"]["verdict"]
                .as_str()
                .expect("verdict is a string"),
        )
        .expect("verdict is contract vocabulary");

        assert_eq!(verdict, GateVerdict::Pass);
        assert_eq!(gate_exit_code(verdict, true), 0);
        assert_eq!(gate_exit_code(verdict, false), 0);
    }

    #[test]
    fn introduced_warning_uses_typed_operator_policy_exit_lane() {
        // The canonical verdict remains CONDITIONAL, while the orthogonal 2.3
        // disposition lets default strict accept a warning and preserves the
        // explicit warnings-clean exit 2.
        use crate::gate::{GateVerdict, gate_exit_code_for_disposition};
        use crate::policy::engine::{EnforcementDisposition, EnforcementMode};

        let gate = run_gate_with_rustfmt_warning(true);
        let verdict = GateVerdict::try_from(
            gate["decision"]["verdict"]
                .as_str()
                .expect("verdict is a string"),
        )
        .expect("verdict is contract vocabulary");
        let disposition: EnforcementDisposition =
            serde_json::from_value(gate["decision"]["enforcement_disposition"].clone()).unwrap();

        assert_eq!(verdict, GateVerdict::Conditional);
        assert_eq!(disposition, EnforcementDisposition::WarningsOnly);
        assert_eq!(
            gate_exit_code_for_disposition(disposition, EnforcementMode::GateStrict),
            0
        );
        assert_eq!(
            gate_exit_code_for_disposition(disposition, EnforcementMode::GateFailOnWarnings),
            2
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

    fn breaking_removed_symbol() -> BreakingFinding {
        use crate::artifacts::signal::BreakingRisk;
        BreakingFinding {
            file: "src/lib.rs".to_string(),
            kind: BreakingKind::RemovedSymbol {
                symbol_type: "fn".to_string(),
            },
            line: "pub fn old_api()".to_string(),
            risk_level: BreakingRisk::High,
        }
    }

    fn run_gate_with_breaking(escalation: bool) -> serde_json::Value {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = test_config();
        config.breaking_escalation = escalation;
        // No failing checks and no inline findings: without escalation this is a
        // clean PASS, so any CONDITIONAL comes solely from the breaking change.
        let inline = InlineFindingsSummary {
            status: "passed".to_string(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();
        let breaking = vec![breaking_removed_symbol()];

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            checks: &[],
            heuristics: None,
            inline: &inline,
            breaking: &breaking,
            rust_api_delta: None,
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

    fn run_gate_with_warning_and_breaking(escalation: bool) -> serde_json::Value {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = test_config();
        config.breaking_escalation = escalation;
        config.policy.default_severity = crate::policy::PolicySeverity::Ignore;
        config
            .policy
            .checks
            .insert("rustfmt".to_string(), crate::policy::PolicySeverity::Warn);
        let checks = vec![rustfmt_warnings_check()];
        let inline = InlineFindingsSummary {
            status: "passed".to_string(),
            findings_count: 0,
            dashboard_findings: vec![],
        };
        let coverage = empty_coverage();
        let (resolved_target, resolved_bases) = resolved_refs();
        let breaking = vec![breaking_removed_symbol()];

        generate_merge_gate(MergeGateInput {
            dir: tmp.path(),
            config: &config,
            ledger: &empty_ledger(),
            checks: &checks,
            heuristics: None,
            inline: &inline,
            breaking: &breaking,
            rust_api_delta: None,
            coverage: &coverage,
            diffs: &[],
            skipped_checks: &[],
            resolved_target: &resolved_target,
            resolved_bases: &resolved_bases,
            clean_comparison: CleanComparison::for_test(true, true),
        })
        .expect("merge gate");

        serde_json::from_slice(&std::fs::read(tmp.path().join("MERGE_GATE.json")).unwrap()).unwrap()
    }

    #[test]
    fn operator_policy_mixed_warning_and_potential_breaking_stays_enforced() {
        use crate::gate::gate_exit_code_for_disposition;
        use crate::policy::engine::{EnforcementDisposition, EnforcementMode};

        let enabled = run_gate_with_warning_and_breaking(true);
        let disposition: EnforcementDisposition =
            serde_json::from_value(enabled["decision"]["enforcement_disposition"].clone()).unwrap();
        assert_eq!(disposition, EnforcementDisposition::ReviewRequired);
        assert_eq!(
            gate_exit_code_for_disposition(disposition, EnforcementMode::GateStrict),
            2
        );

        let disabled = run_gate_with_warning_and_breaking(false);
        let disposition: EnforcementDisposition =
            serde_json::from_value(disabled["decision"]["enforcement_disposition"].clone())
                .unwrap();
        assert_eq!(disposition, EnforcementDisposition::WarningsOnly);
        assert_eq!(
            gate_exit_code_for_disposition(disposition, EnforcementMode::GateStrict),
            0
        );
    }

    #[test]
    fn breaking_change_escalates_verdict_to_conditional_when_knob_on() {
        // critic-1: a genuine breaking API change must raise PASS → CONDITIONAL.
        let gate = run_gate_with_breaking(true);

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("CONDITIONAL"));
        assert_eq!(
            gate["decision"]["merge_recommendation"].as_str(),
            Some("review_required")
        );
        assert_eq!(gate["decision"]["allow_merge"].as_bool(), Some(false));
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item
                    .as_str()
                    .is_some_and(|text| text == "breaking API change detected: 1 finding"))),
            "escalation reason must be surfaced as a review caveat"
        );
    }

    #[test]
    fn breaking_change_stays_pass_with_informational_caveat_when_knob_off() {
        // Knob off: no verdict escalation, but the breaking change is still
        // visible as an informational caveat (from build_review_caveats).
        let gate = run_gate_with_breaking(false);

        assert_eq!(gate["decision"]["verdict"].as_str(), Some("PASS"));
        assert_eq!(gate["decision"]["allow_merge"].as_bool(), Some(true));
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item
                    .as_str()
                    .is_some_and(|text| text.contains("removed public symbol")))),
            "breaking change must remain visible as an informational caveat"
        );
        assert!(
            gate["decision"]["review_caveats"]
                .as_array()
                .is_some_and(|items| items.iter().all(|item| item
                    .as_str()
                    .is_none_or(|text| !text.starts_with("breaking API change detected")))),
            "no escalation reason caveat when the knob is off"
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
